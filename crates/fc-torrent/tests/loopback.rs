//! Two engines on loopback: one seeds a file, the other downloads it directly
//! from the seed. No trackers, no DHT, so nothing leaves this machine.

use std::net::{SocketAddr, TcpListener};
use std::time::{Duration, Instant};

use fc_torrent::{EngineConfig, TorrentEngine, TorrentPhase, TorrentState, TorrentStatus};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
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
async fn wait_for(
    engine: &TorrentEngine,
    id: usize,
    secs: u64,
    done: impl Fn(&TorrentStatus) -> bool,
) -> TorrentStatus {
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

/// A seeding engine with one file, and the `.torrent` for it.
async fn seed_one_file(
    dir: &std::path::Path,
    name: &str,
    len: usize,
) -> (TorrentEngine, usize, Vec<u8>, SocketAddr) {
    let source = dir.join(name);
    std::fs::write(&source, data(len)).unwrap();
    let torrent = TorrentEngine::create_file_torrent(&source, Some(name))
        .await
        .unwrap();
    let port = free_port();
    let seed = TorrentEngine::start(EngineConfig {
        listen_port: Some(port),
        ..EngineConfig::new(dir)
    })
    .await
    .unwrap();
    let id = seed
        .add_torrent_bytes(torrent.clone(), false, vec![])
        .await
        .unwrap();
    wait_for(&seed, id, 60, |s| s.finished).await;
    (seed, id, torrent, SocketAddr::from(([127, 0, 0, 1], port)))
}

#[tokio::test(flavor = "multi_thread")]
async fn a_file_downloads_from_a_seed_on_loopback_and_matches() {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let bytes = data(2 * 1024 * 1024 + 123);
    let (seed, seed_id, torrent, peer) =
        seed_one_file(seed_dir.path(), "data.bin", bytes.len()).await;
    let seeding = seed.status(seed_id).unwrap();
    assert_eq!(seeding.phase(), TorrentPhase::Seeding);

    let leech = TorrentEngine::start(EngineConfig::new(leech_dir.path()))
        .await
        .expect("leech engine");
    let id = leech
        .add_torrent_bytes(torrent, false, vec![peer])
        .await
        .expect("add to leech");
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
    let torrent = TorrentEngine::create_file_torrent(&source, Some("pause.bin"))
        .await
        .unwrap();

    let engine = TorrentEngine::start(EngineConfig::new(dir.path().join("out")))
        .await
        .unwrap();
    let id = engine
        .add_torrent_bytes(torrent, true, vec![])
        .await
        .unwrap();
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
    let (seed, _seed_id, torrent, peer) =
        seed_one_file(seed_dir.path(), "gone.bin", 256 * 1024).await;

    let leech = TorrentEngine::start(EngineConfig::new(leech_dir.path()))
        .await
        .unwrap();
    let id = leech
        .add_torrent_bytes(torrent, false, vec![peer])
        .await
        .unwrap();
    wait_for(&leech, id, 90, |s| s.finished).await;
    let copy = leech_dir.path().join("gone.bin");
    assert!(copy.exists());

    leech.remove(id, true).await.unwrap();
    assert!(
        !copy.exists(),
        "remove with delete_files takes the data with it"
    );
    leech.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_with_no_peers_is_listed_as_waiting_until_removed() {
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::start(EngineConfig::new(dir.path()))
        .await
        .unwrap();
    let magnet = "magnet:?xt=urn:btih:0123456789abcdef0123456789abcdef01234567&dn=Example+Name%21";

    let id = engine.add_magnet(magnet, false, vec![]).await.unwrap();
    let row = engine.status(id).unwrap();
    assert_eq!(row.phase(), TorrentPhase::WaitingForMetadata);
    assert_eq!(row.info_hash, "0123456789abcdef0123456789abcdef01234567");
    assert_eq!(
        row.name.as_deref(),
        Some("Example Name!"),
        "display name is decoded"
    );
    assert!(engine.list().iter().any(|t| t.id == id));

    // Waiting for metadata is pausable too; pausing cancels discovery while
    // retaining the row and its identity for a later resume.
    engine.pause(id).await.unwrap();
    let paused = engine.status(id).unwrap();
    assert_eq!(paused.phase(), TorrentPhase::Paused);
    assert!(paused.metadata_pending);
    assert_eq!(paused.info_hash, row.info_hash);

    // Resume restarts metadata discovery under the same id. With no peers and
    // discovery disabled it may quickly report an error, so assert identity
    // and availability rather than a transient phase.
    engine.resume(id).await.unwrap();
    assert_eq!(engine.status(id).unwrap().id, id);
    engine.remove(id, false).await.unwrap();
    assert!(engine.list().iter().all(|t| t.id != id));
    engine.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_resolves_into_a_download_under_the_same_id() {
    let seed_dir = tempfile::tempdir().unwrap();
    let leech_dir = tempfile::tempdir().unwrap();
    let bytes = data(300 * 1024 + 7);
    let (seed, seed_id, _torrent, peer) =
        seed_one_file(seed_dir.path(), "from-magnet.bin", bytes.len()).await;
    let hash = seed.status(seed_id).unwrap().info_hash;

    let leech = TorrentEngine::start(EngineConfig::new(leech_dir.path()))
        .await
        .unwrap();
    let magnet = format!("magnet:?xt=urn:btih:{hash}&dn=from-magnet.bin");
    let id = leech.add_magnet(&magnet, false, vec![peer]).await.unwrap();

    // The file list comes from the seed; the transfer then keeps the same id.
    let done = wait_for(&leech, id, 90, |s| s.finished).await;
    assert!(!done.metadata_pending);
    assert_eq!(done.name.as_deref(), Some("from-magnet.bin"));
    assert_eq!(
        leech.list().iter().filter(|t| t.id == id).count(),
        1,
        "one row, not a waiting row plus a transfer"
    );
    let got = std::fs::read(leech_dir.path().join("from-magnet.bin")).unwrap();
    assert_eq!(got, bytes);

    leech.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_waiting_magnet_survives_a_restart_until_it_is_removed() {
    let out = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let config = || EngineConfig {
        state_dir: Some(state.path().to_path_buf()),
        ..EngineConfig::new(out.path())
    };
    let magnet = "magnet:?xt=urn:btih:89abcdef0123456789abcdef0123456789abcdef&dn=later";

    let first = TorrentEngine::start(config()).await.unwrap();
    first.add_magnet(magnet, false, vec![]).await.unwrap();
    first.shutdown().await;

    let second = TorrentEngine::start(config()).await.unwrap();
    let rows = second.list();
    assert_eq!(rows.len(), 1, "the waiting magnet came back");
    assert_eq!(rows[0].phase(), TorrentPhase::WaitingForMetadata);
    assert_eq!(rows[0].name.as_deref(), Some("later"));

    second.remove(rows[0].id, false).await.unwrap();
    second.shutdown().await;

    let third = TorrentEngine::start(config()).await.unwrap();
    assert!(
        third.list().is_empty(),
        "a removed magnet does not come back"
    );
    third.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_and_its_paused_state_survive_a_restart() {
    let out = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    let config = || EngineConfig {
        state_dir: Some(state.path().to_path_buf()),
        ..EngineConfig::new(out.path())
    };
    // Two different files: adding the same torrent twice returns the transfer already there.
    let kept = out.path().join("kept.bin");
    std::fs::write(&kept, data(200 * 1024)).unwrap();
    let kept_torrent = TorrentEngine::create_file_torrent(&kept, Some("kept.bin"))
        .await
        .unwrap();
    let held = out.path().join("held.bin");
    std::fs::write(&held, data(150 * 1024 + 3)).unwrap();
    let held_torrent = TorrentEngine::create_file_torrent(&held, Some("held.bin"))
        .await
        .unwrap();

    let first = TorrentEngine::start(config()).await.unwrap();
    let seeding_id = first
        .add_torrent_bytes(kept_torrent, false, vec![])
        .await
        .unwrap();
    wait_for(&first, seeding_id, 60, |s| s.finished).await;
    let paused_id = first
        .add_torrent_bytes(held_torrent, true, vec![])
        .await
        .unwrap();
    assert_ne!(paused_id, seeding_id);
    wait_for(&first, paused_id, 30, |s| s.state == TorrentState::Paused).await;
    first.shutdown().await;

    let second = TorrentEngine::start(config()).await.unwrap();
    let seeding = wait_for(&second, seeding_id, 60, |s| s.finished).await;
    assert_eq!(
        seeding.phase(),
        TorrentPhase::Seeding,
        "the finished download is still a seed"
    );
    // It first checks the data on disk again, then settles back into paused.
    let held = wait_for(&second, paused_id, 60, |s| s.state == TorrentState::Paused).await;
    assert_eq!(held.phase(), TorrentPhase::Paused, "paused stays paused");
    second.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_download_goes_to_the_folder_chosen_for_it() {
    let seed_dir = tempfile::tempdir().unwrap();
    let engine_dir = tempfile::tempdir().unwrap();
    let chosen = tempfile::tempdir().unwrap();
    let (seed, _seed_id, torrent, peer) =
        seed_one_file(seed_dir.path(), "chosen.bin", 64 * 1024).await;

    let leech = TorrentEngine::start(EngineConfig::new(engine_dir.path()))
        .await
        .unwrap();
    let opts = fc_torrent::AddOptions {
        peers: vec![peer],
        output: Some(chosen.path().to_path_buf()),
        ..Default::default()
    };
    let id = leech.add_torrent_bytes_with(torrent, opts).await.unwrap();
    wait_for(&leech, id, 90, |s| s.finished).await;

    assert!(
        chosen.path().join("chosen.bin").exists(),
        "the file is in the chosen folder"
    );
    assert!(
        !engine_dir.path().join("chosen.bin").exists(),
        "and not in the engine's default folder"
    );
    leech.shutdown().await;
    seed.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_magnet_link_without_an_info_hash_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let engine = TorrentEngine::start(EngineConfig::new(dir.path()))
        .await
        .unwrap();
    assert!(
        engine
            .add_magnet("https://example.com", false, vec![])
            .await
            .is_err()
    );
    assert!(
        engine
            .add_magnet("magnet:?dn=no-hash", false, vec![])
            .await
            .is_err()
    );
    engine.shutdown().await;
}
