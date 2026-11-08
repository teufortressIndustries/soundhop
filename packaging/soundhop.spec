Name:           soundhop
Version:        2.0.0
Release:        1%{?dist}
Summary:        Seamless cross-distro local network audio sharing and multi-room broadcast

License:        MIT
URL:            https://github.com/teufortressIndustries/soundhop
Source0:        %{name}-%{version}.tar.gz

BuildRequires:  cargo
BuildRequires:  rust
BuildRequires:  gtk3-devel
BuildRequires:  libappindicator-gtk3-devel
BuildRequires:  libxdo-devel
BuildRequires:  libxcb-devel
BuildRequires:  libxkbcommon-devel
BuildRequires:  wayland-devel
BuildRequires:  mesa-libGL-devel
BuildRequires:  openssl-devel

Requires:       pipewire-pulseaudio
Requires:       gtk3
Requires:       libappindicator-gtk3
Requires:       libxdo

%description
SoundHop v2.0 lets you seamlessly stream and share audio between Linux and Windows
devices over your local network using native PipeWire/PulseAudio protocol tunnels,
multi-room combine sinks, persistent per-app routing rules, PIN/PSK HMAC pairing,
an interactive terminal UI (TUI), and a native system tray.

%prep
%autosetup -n %{name}-%{version}

%build
cargo build --release --workspace

%install
install -Dpm 0755 target/release/soundhop %{buildroot}%{_bindir}/soundhop
install -Dpm 0644 packaging/io.github.soundhop.desktop %{buildroot}%{_datadir}/applications/io.github.soundhop.desktop
install -Dpm 0644 packaging/io.github.soundhop.metainfo.xml %{buildroot}%{_datadir}/metainfo/io.github.soundhop.appdata.xml
install -Dpm 0644 assets/soundhop.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg
install -Dpm 0644 assets/soundhop.png %{buildroot}%{_datadir}/icons/hicolor/256x256/apps/soundhop.png

%files
%license LICENSE
%doc README.md
%{_bindir}/soundhop
%{_datadir}/applications/io.github.soundhop.desktop
%{_datadir}/metainfo/io.github.soundhop.appdata.xml
%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg
%{_datadir}/icons/hicolor/256x256/apps/soundhop.png

%changelog
* Sun Nov 08 2026 SoundHop Contributors <soundhop@example.com> - 2.0.0-1
- Complete Rust rewrite with TUI, Multi-Room Broadcast, Persistent App Routing, PIN/PSK Pairing, and Live Telemetry.
