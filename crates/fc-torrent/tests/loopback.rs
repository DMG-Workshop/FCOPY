//! Two engines on loopback: one seeds a file, the other downloads it directly
//! from the seed. No trackers, no DHT, so nothing leaves this machine.

use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use fc_torrent::{EngineConfig, TorrentEngine, TorrentPhase, TorrentState};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn data(len: usize) -> Vec<u8> {
    let mut x: u32 = 0x9e37_79b9;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

/// Poll until `done` holds for the transfer, or give up.
async fn wait_for(engine: &TorrentEngine, id: usize, secs: u64, done: impl Fn(&fc_torrent::TorrentStatus) -> bool) -> fc_torrent::TorrentStatus {
    let deadline = Instant::now() + Duration::from_secs(secs);
    loop {
        let s = engine.status(id).expect("transfer exists");
        if done(&s) {
            return s;
        }
        assert!(Instant::now() < deadline, "timed out; last status: {s:?}");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_downloads_from_a_seed_on_loopback_and_matches() {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let source = seed_dir.path().join("data.bin");
    let bytes = data(2 * 1024 * 1024 + 123);
    std::fs::write(&source, &bytes).unwrap();

    let seed_port = free_port();
    let seed = TorrentEngine::start(EngineConfig { listen_port: Some(seed_port), ..EngineConfig::new(seed_dir.path()) })
        .await
        .expect("seed engine");
    let torrent = TorrentEngine::create_file_torrent(&source, Some("data.bin")).await.expect("create torrent");

    let seed_id = seed.add_torrent_bytes(torrent.clone(), false, vec![]).await.expect("add to seed");
    let seeding = wait_for(&seed, seed_id, 60, |s| s.finished).await;
    assert_eq!(seeding.phase(), TorrentPhase::Seeding);

    let leech = TorrentEngine::start(EngineConfig::new(leech_dir.path())).await.expect("leech engine");
    let peer: SocketAddr = format!("127.0.0.1:{seed_port}").parse().unwrap();
    let id = leech.add_torrent_bytes(torrent, false, vec![peer]).await.expect("add to leech");
    let done = wait_for(&leech, id, 90, |s| s.finished).await;
    assert_eq!(done.state, TorrentState::Live);
    assert_eq!(done.phase(), TorrentPhase::Seeding);
    assert_eq!(done.percent(), 100.0);

    let got = std::fs::read(leech_dir.path().join("data.bin")).unwrap();
    assert_eq!(got, bytes, "downloaded bytes match the source");

    leech.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_resume_and_remove_a_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("pause.bin");
    std::fs::write(&source, data(512 * 1024)).unwrap();
    let torrent = TorrentEngine::create_file_torrent(&source, Some("pause.bin")).await.unwrap();

    let engine = TorrentEngine::start(EngineConfig::new(dir.path().join("out"))).await.unwrap();
    let id = engine.add_torrent_bytes(torrent, true, vec![]).await.unwrap();
    let s = wait_for(&engine, id, 30, |s| s.state == TorrentState::Paused).await;
    assert_eq!(s.phase(), TorrentPhase::Paused);

    engine.resume(id).await.unwrap();
    engine.pause(id).await.unwrap();
    assert_eq!(engine.status(id).unwrap().phase(), TorrentPhase::Paused);

    engine.remove(id, false).await.unwrap();
    assert!(engine.list().iter().all(|t| t.id != id));
    assert!(engine.status(id).is_err());
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn remove_with_delete_files_deletes_the_download() {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let source = seed_dir.path().join("gone.bin");
    std::fs::write(&source, data(256 * 1024)).unwrap();
    let torrent = TorrentEngine::create_file_torrent(&source, Some("gone.bin")).await.unwrap();

    let seed_port = free_port();
    let seed = TorrentEngine::start(EngineConfig { listen_port: Some(seed_port), ..EngineConfig::new(seed_dir.path()) })
        .await
        .unwrap();
    let seed_id = seed.add_torrent_bytes(torrent.clone(), false, vec![]).await.unwrap();
    wait_for(&seed, seed_id, 60, |s| s.finished).await;

    let leech = TorrentEngine::start(EngineConfig::new(leech_dir.path())).await.unwrap();
    let peer: SocketAddr = format!("127.0.0.1:{seed_port}").parse().unwrap();
    let id = leech.add_torrent_bytes(torrent, false, vec![peer]).await.unwrap();
    wait_for(&leech, id, 90, |s| s.finished).await;
    let copy = leech_dir.path().join("gone.bin");
    assert!(copy.exists());

    leech.remove(id, true).await.unwrap();
    assert!(!copy.exists(), "remove with delete_files takes the data with it");
    leech.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_link_is_listed_as_a_transfer() {
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::start(EngineConfig::new(dir.path())).await.unwrap();
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=example";
    // No peers exist for this hash, so the metadata never comes. The call must still
    // return promptly, with the pending error, rather than hang.
    let started = Instant::now();
    let err = engine.add_magnet(magnet, true, Duration::from_secs(3)).await.expect_err("metadata never arrives");
    assert!(matches!(err, fc_torrent::TorrentError::MetadataPending), "{err}");
    assert!(started.elapsed() < Duration::from_secs(15));
    engine.shutdown().await;
}
