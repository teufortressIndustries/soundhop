Name:           soundhop
Version:        1.1.0
Release:        1%{?dist}
Summary:        Seamless cross-distro local network audio sharing for Linux

License:        MIT
URL:            https://github.com/example/soundhop
Source0:        %{name}-%{version}.tar.gz

BuildArch:      noarch
BuildRequires:  python3-devel
BuildRequires:  pyproject-rpm-macros
BuildRequires:  python3-build
BuildRequires:  python3-installer
BuildRequires:  python3-wheel
BuildRequires:  python3-setuptools

Requires:       python3 >= 3.10
Requires:       pipewire-pulseaudio
Recommends:     python3-zeroconf
Recommends:     python3-pyside6

%description
SoundHop lets you seamlessly stream and share audio between Linux devices
(such as Fedora, CachyOS/Arch, Ubuntu, openSUSE) over your local network using
native PipeWire/PulseAudio protocol tunnels and zero-config UDP/mDNS discovery.
Includes both a powerful CLI and an optional lightweight system tray for KDE/GNOME.

%prep
%autosetup -n %{name}-%{version}

%build
%pyproject_wheel

%install
%pyproject_install
%pyproject_save_files soundhop

install -Dpm 0644 packaging/io.github.soundhop.desktop %{buildroot}%{_datadir}/applications/io.github.soundhop.desktop
install -Dpm 0644 src/soundhop/assets/soundhop.svg %{buildroot}%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg

%files -f %{pyproject_files}
%license LICENSE
%doc README.md
%{_bindir}/soundhop
%{_bindir}/shop
%{_datadir}/applications/io.github.soundhop.desktop
%{_datadir}/icons/hicolor/scalable/apps/soundhop.svg

%changelog
* Mon Nov 02 2026 SoundHop Contributors <soundhop@example.com> - 1.0.0-1
- Initial release of SoundHop cross-distro audio sharing.
