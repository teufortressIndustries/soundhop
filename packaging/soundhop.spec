Name:           soundhop
Version:        2.6.2
Release:        1%{?dist}
Summary:        Seamless cross-distro local network audio sharing and multi-room broadcast

License:        MIT
URL:            https://github.com/teufortressIndustries/soundhop
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  pkgconfig
BuildRequires:  gstreamer1-devel
BuildRequires:  gstreamer1-plugins-base-devel
BuildRequires:  gtk3-devel
BuildRequires:  libappindicator-gtk3-devel
BuildRequires:  libxdo-devel
BuildRequires:  libxcb-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  wayland-devel
BuildRequires:  mesa-libGL-devel

Requires:       pipewire-pulseaudio
Requires:       gstreamer1
Requires:       gstreamer1-plugins-base
# gstreamer1-plugins-good provides libgstpulseaudio.so, rtp, rtpmanager, udp, and volume on Fedora
Requires:       gstreamer1-plugins-good
Requires:       gstreamer1-plugins-bad-free
Requires:       gtk3
Requires:       libappindicator-gtk3
Requires:       libxdo

%description
SoundHop lets you seamlessly stream and share audio between Linux and Windows
devices over your local network using native PipeWire/PulseAudio protocol tunnels,
multi-room combine sinks, persistent per-app routing rules, PIN/PSK HMAC pairing,
an interactive terminal UI (TUI), and a native desktop GUI and system tray.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --workspace --locked

%install
install -Dpm 0755 target/release/soundhop %{buildroot}%{_bindir}/soundhop
install -Dpm 0755 target/release/soundhop-gui %{buildroot}%{_bindir}/soundhop-gui
install -Dpm 0644 packaging/io.github.soundhop.desktop %{buildroot}%{_datadir}/applications/io.github.soundhop.desktop
install -Dpm 0644 packaging/io.github.soundhop.metainfo.xml %{buildroot}%{_datadir}/metainfo/io.github.soundhop.metainfo.xml
install -Dpm 0644 assets/soundhop.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg
install -Dpm 0644 assets/soundhop.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/soundhop.png

%files
%license LICENSE
%doc README.md CHANGELOG.md
%{_bindir}/soundhop
%{_bindir}/soundhop-gui
%{_datadir}/applications/io.github.soundhop.desktop
%{_datadir}/metainfo/io.github.soundhop.metainfo.xml
%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg
%{_datadir}/icons/hicolor/256x256/apps/soundhop.png

%changelog
* Fri Nov 20 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.4.0-1
- Release 2.4.0 with optional headless CLI feature, aarch64 builds, packaging polish, and hardening.

* Fri Nov 20 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.3.1-1
- Release 2.3.1 with AppManager update metadata and CI version checks.

* Fri Nov 20 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.3.0-1
- Release 2.3.0 with Windows Inno Setup installer, portable ZIP, DX12 renderer, and WASAPI hotplug recovery.

* Thu Nov 19 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.2.0-1
- Release 2.2.0 with xtask version sync and Windows audio backend improvements.

* Tue Nov 17 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.1.1-1
- Release 2.1.1 with packaging and CI alignment.

* Sun Nov 08 2026 SoundHop Contributors <https://github.com/teufortressIndustries/soundhop> - 2.0.0-1
- Complete Rust rewrite with TUI, Multi-Room Broadcast, Persistent App Routing, PIN/PSK Pairing, and Live Telemetry.
