%global debug_package %{nil}

Name:           fcopy
Version:        %{fcopy_version}
Release:        1
Summary:        Verified file copying with a desktop queue and CLI
License:        MIT OR Apache-2.0
URL:            https://github.com/DMG-Workshop/FCOPY
# ELF dependencies are generated in addition to these dlopen/runtime dependencies.
Requires:       ca-certificates
Requires:       libX11.so.6()(64bit), libXcursor.so.1()(64bit)
Requires:       libXi.so.6()(64bit), libXrandr.so.2()(64bit)
Requires:       libxkbcommon.so.0()(64bit), libxkbcommon-x11.so.0()(64bit)
Requires:       libwayland-client.so.0()(64bit), libwayland-cursor.so.0()(64bit)
Requires:       libwayland-egl.so.1()(64bit), libEGL.so.1()(64bit), libGL.so.1()(64bit)
Requires:       hicolor-icon-theme

%description
Copy files locally or between paired PCs with checksums, pause/resume,
bandwidth limits, transfer history and explicit recovery of unfinished jobs.

%prep

%build

%install
mkdir -p "%{buildroot}"
cp -a "%{fcopy_stage}/." "%{buildroot}/"

%files
%{_bindir}/fcopy
%{_bindir}/fcopy-gui
%{_datadir}/applications/io.github.fcopy.desktop
%{_datadir}/metainfo/io.github.fcopy.metainfo.xml
%{_datadir}/kio/servicemenus/fcopy.desktop
%{_datadir}/nemo/actions/fcopy.nemo_action
%{_datadir}/icons/hicolor/scalable/apps/io.github.fcopy.svg
%doc %{_datadir}/doc/fcopy/README.md
