#!/usr/bin/env bash
# Build and install floptle-relay as a service on a fresh cloud instance.
# Works on Ubuntu (apt) and Oracle Linux / RHEL / Fedora (dnf), ARM or x86 —
# which matters because Oracle's image and shape pickers filter each other,
# and the shape that actually has free capacity often dictates the distro you
# end up on. Run as a normal sudo user:
#
#     bash setup-relay.sh https://github.com/Fopull-LLC/Floptle.git
#
# The repo is PRIVATE, so either pass an authenticated URL, or clone it
# yourself first and run this from inside the checkout with no argument.
set -euo pipefail

PORT="${PORT:-7788}"
REPO="${1:-}"
SRC="$HOME/floptle"

# Which package manager and firewall this box uses. Everything below branches
# on this once, rather than assuming a distro that capacity limits may have
# chosen for us.
if command -v apt-get >/dev/null; then
  PKG=apt
elif command -v dnf >/dev/null; then
  PKG=dnf
else
  echo "unsupported distro: need apt-get or dnf" >&2
  exit 1
fi
echo "==> packages ($PKG)"
case "$PKG" in
  apt)
    sudo apt-get update -qq
    sudo apt-get install -y -qq build-essential pkg-config git curl
    ;;
  dnf)
    sudo dnf install -y -q gcc make pkgconf-pkg-config git curl
    ;;
esac

# A relay needs ~1 GB to link. Oracle's always-free x86 shape has exactly 1 GB
# and no swap, so cargo gets OOM-killed most of the way through the build —
# which looks like a mysterious "signal: 9" rather than anything about memory.
TOTAL_MB=$(free -m | awk '/^Mem:/{print $2}')
SWAP_MB=$(free -m | awk '/^Swap:/{print $2}')
if [ "$TOTAL_MB" -lt 2048 ] && [ "$SWAP_MB" -lt 1024 ] && [ ! -f /swapfile ]; then
  echo "==> only ${TOTAL_MB}MB RAM and no swap — adding 2G so the build survives"
  sudo fallocate -l 2G /swapfile || sudo dd if=/dev/zero of=/swapfile bs=1M count=2048
  sudo chmod 600 /swapfile
  sudo mkswap -q /swapfile
  sudo swapon /swapfile
  grep -q '^/swapfile' /etc/fstab || echo '/swapfile none swap sw 0 0' | sudo tee -a /etc/fstab >/dev/null
fi

echo "==> rust"
if ! command -v cargo >/dev/null; then
  curl -sSf https://sh.rustup.rs | sh -s -- -y --profile minimal
  . "$HOME/.cargo/env"
fi
. "$HOME/.cargo/env"

echo "==> source"
if [ -n "$REPO" ]; then
  [ -d "$SRC" ] || git clone --depth 1 "$REPO" "$SRC"
  git -C "$SRC" pull --ff-only || true
else
  SRC="$PWD"
fi

# The relay depends only on floptle-net — no GPU, no windowing, no audio. This
# is why it builds on a bare headless box with nothing but build-essential.
echo "==> build (a few minutes on a free-tier core)"
cargo build --release -p floptle-relay --manifest-path "$SRC/Cargo.toml"
sudo install -m755 "$SRC/target/release/floptle-relay" /usr/local/bin/floptle-relay

echo "==> firewall"
# Oracle's images ship a REJECT-everything host firewall that survives opening
# the port in the web console. BOTH have to be done; this is the half that gets
# forgotten, and the symptom is a lobby code nobody can ever join.
if command -v firewall-cmd >/dev/null && sudo firewall-cmd --state >/dev/null 2>&1; then
  sudo firewall-cmd --permanent --add-port="${PORT}/udp"
  sudo firewall-cmd --reload
else
  sudo iptables -I INPUT 1 -p udp --dport "$PORT" -j ACCEPT
  # Persist across reboot, however this distro spells it.
  if command -v netfilter-persistent >/dev/null; then
    sudo netfilter-persistent save
  elif [ "$PKG" = apt ]; then
    sudo DEBIAN_FRONTEND=noninteractive apt-get install -y -qq iptables-persistent \
      && sudo netfilter-persistent save
  else
    sudo dnf install -y -q iptables-services 2>/dev/null \
      && sudo service iptables save 2>/dev/null || true
  fi
fi

echo "==> service"
sudo tee /etc/systemd/system/floptle-relay.service >/dev/null <<UNIT
[Unit]
Description=Floptle rendezvous relay
After=network-online.target
Wants=network-online.target

[Service]
ExecStart=/usr/local/bin/floptle-relay ${PORT}
Restart=always
RestartSec=2
# It forwards opaque bytes between strangers, so give it nothing to lose:
# no privileges, no writable filesystem, its own empty /tmp.
DynamicUser=yes
NoNewPrivileges=yes
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
RestrictAddressFamilies=AF_INET AF_INET6
MemoryMax=256M

[Install]
WantedBy=multi-user.target
UNIT

sudo systemctl daemon-reload
sudo systemctl enable --now floptle-relay
sleep 2
sudo systemctl --no-pager --lines=15 status floptle-relay || true

IP="$(curl -s https://api.ipify.org || echo '<this-box>')"
cat <<DONE

==> done

  Relay:  ${IP}:${PORT}

  In Floptle, set the 🌐 panel's relay field to that, or from Lua:
      net.host{ relay = "${IP}:${PORT}" }
      net.join("relay://${IP}:${PORT}/CODE")

  Still to do in the Oracle web console:
      Networking > VCN > Security List > Add Ingress Rule
      Source 0.0.0.0/0 · IP Protocol UDP · Destination port ${PORT}

  Logs:   sudo journalctl -u floptle-relay -f
  Check:  sudo ss -ulnp | grep ${PORT}
DONE
