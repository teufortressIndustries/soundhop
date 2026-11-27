FROM archlinux:base-devel

ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH="/usr/local/cargo/bin:${PATH}"

RUN pacman -Sy --noconfirm --needed archlinux-keyring && \
    pacman -Syu --noconfirm --needed \
      base-devel rustup pkgconf wget curl file strace binutils patchelf zsync \
      squashfs-tools desktop-file-utils appstream xorg-server-xvfb \
      gtk3 libayatana-appindicator xdotool libxcb libxcursor libxi libxrandr \
      libxkbcommon libxkbcommon-x11 wayland mesa lld \
      gstreamer gst-plugins-base-libs gst-plugins-base gst-plugins-good gst-plugins-bad \
      git && \
    pacman -Scc --noconfirm && \
    rustup default stable
