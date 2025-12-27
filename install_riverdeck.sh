#!/usr/bin/env bash
set -euo pipefail

GITHUB_REPO="sulrwin/RiverDeck"
FLATHUB_APP_ID="io.github.sulrwin.riverdeck"
UDEV_RULES_URL_BASE="https://raw.githubusercontent.com/sulrwin/RiverDeck"

# Cached GitHub release metadata (populated lazily).
LATEST_RELEASE_JSON=""
LATEST_TAG=""

if [ -t 1 ]; then
    RED="\033[0;31m"
    GREEN="\033[0;32m"
    YELLOW="\033[0;33m"
    BLUE="\033[0;34m"
    BOLD="\033[1m"
    RESET="\033[0m"
else
    RED=""
    GREEN=""
    YELLOW=""
    BLUE=""
    RESET=""
fi

trap 'echo -e "${YELLOW}Need help? Open an issue: ${BLUE}https://github.com/'"${GITHUB_REPO}"'/issues${RESET}"' EXIT

msg_info() { echo -e "${BLUE}[*]${RESET} $*"; }
msg_ok() { echo -e "${GREEN}[✓] $*${RESET}"; }
msg_warn() { echo -e "${YELLOW}[!] $*${RESET}"; }
msg_error() { echo -e "${RED}[✗] $*${RESET}" >&2; }

has_cmd() { command -v "$1" >/dev/null 2>&1; }

download_text() {
    local url="$1"
    if has_cmd curl; then
        curl --fail --location --proto '=https' --tlsv1.2 \
            --retry 3 --retry-delay 3 \
            -H "Accept: application/vnd.github+json" \
            -H "User-Agent: riverdeck-install-script" \
            -sS "$url"
    else
        wget -qO- --tries=3 "$url"
    fi
}

github_latest_release_json() {
    if [ -n "${LATEST_RELEASE_JSON}" ]; then
        echo "${LATEST_RELEASE_JSON}"
        return 0
    fi
    local api_url="https://api.github.com/repos/${GITHUB_REPO}/releases/latest"
    LATEST_RELEASE_JSON="$(download_text "$api_url" || true)"
    if [ -z "${LATEST_RELEASE_JSON}" ]; then
        msg_error "Failed to fetch GitHub release metadata"
        return 1
    fi
    echo "${LATEST_RELEASE_JSON}"
}

github_latest_tag() {
    if [ -n "${LATEST_TAG}" ]; then
        echo "${LATEST_TAG}"
        return 0
    fi
    local json
    json="$(github_latest_release_json)" || return 1
    if has_cmd jq; then
        LATEST_TAG="$(echo "$json" | jq -r '.tag_name // empty' | head -n1)"
    else
        # Best-effort fallback if jq isn't installed.
        LATEST_TAG="$(echo "$json" | grep -Eo '"tag_name"\s*:\s*"[^"]+"' | head -n1 | sed -E 's/.*"([^"]+)".*/\1/')"
    fi
    if [ -z "${LATEST_TAG}" ]; then
        msg_error "Failed to determine latest release tag"
        return 1
    fi
    echo "${LATEST_TAG}"
}

confirm() {
    printf "%b$1 [y/N]: %b" "$YELLOW$BOLD" "$RESET"
    read -r ans
    case "$ans" in
    [yY] | [yY][eE][sS]) return 0 ;;
    *) return 1 ;;
    esac
}

detect_family() {
    if has_cmd rpm-ostree || grep -qi "universal blue" /etc/os-release 2>/dev/null; then
        echo "ublue"
    elif has_cmd pacman; then
        echo "arch"
    elif has_cmd zypper || has_cmd dnf || has_cmd rpm; then
        echo "rpm"
    elif has_cmd apt-get || has_cmd dpkg; then
        echo "debian"
    else
        echo "unknown"
    fi
}

fetch_latest_asset() {
    local ext="$1"
    local arch arch_deb arch_rpm arch_pattern download_url

    arch="$(uname -m)"
    case "$arch" in
    x86_64)
        arch_deb="amd64"
        arch_rpm="x86_64"
        ;;
    aarch64)
        arch_deb="arm64"
        arch_rpm="aarch64"
        ;;
    *)
        msg_error "Unsupported architecture $arch"
        exit 1
        ;;
    esac

    if [ "$ext" = "deb" ]; then
        arch_pattern="${arch_deb}"
    else
        arch_pattern="${arch_rpm}"
    fi

    local json
    json="$(github_latest_release_json)" || return 1

    if has_cmd jq; then
        download_url="$(echo "$json" | jq -r --arg arch "${arch_pattern}" --arg ext "${ext}" '
            .assets[].browser_download_url
            | select(test($arch + "\\\\." + $ext + "($|\\\\?)"))
        ' | head -n1 || true)"
    else
        # Fallback: parse URLs from JSON with grep. Less robust than jq but works without extra deps.
        download_url="$(echo "$json" |
            grep -Eo "https://[^ \"]+${arch_pattern}\.${ext}([^\"]*)" |
            head -n1 || true)"
    fi

    if [ -n "$download_url" ]; then
        echo "$download_url"
    else
        msg_error "Failed to find a .${ext} release asset for arch $arch"
        return 1
    fi
}

download_with_retry() {
    local url="$1"
    local dest="$2"

    if has_cmd curl; then
        curl --fail --location --proto '=https' --tlsv1.2 \
            --retry 3 --retry-delay 3 \
            -o "$dest" "$url"
    else
        wget --tries=3 -O "$dest" "$url"
    fi
}

find_sha256_asset_url() {
    # Try to locate a checksum asset for the latest release.
    local json
    json="$(github_latest_release_json)" || return 1
    if has_cmd jq; then
        echo "$json" | jq -r '
            .assets[].browser_download_url
            | select(test("(SHA256SUMS|\\\\.sha256(\\\\.txt)?|\\\\.sha256sum)$"))
        ' | head -n1
    else
        echo "$json" | grep -Eo 'https://[^ "]+(SHA256SUMS|\.sha256(\.txt)?|\.sha256sum)([^"]*)' | head -n1
    fi
}

verify_sha256_if_possible() {
    local file_path="$1"
    local url checksum_tmp base_name
    base_name="$(basename "$file_path")"

    if ! has_cmd sha256sum; then
        msg_warn "sha256sum not found; skipping checksum verification"
        return 0
    fi

    url="$(find_sha256_asset_url || true)"
    if [ -z "${url}" ]; then
        msg_warn "No SHA256 checksum asset found for the latest release"
        if confirm "Continue without checksum verification?"; then
            return 0
        fi
        msg_error "Aborted"
        exit 1
    fi

    checksum_tmp="$(mktemp --suffix=.sha256)"
    download_with_retry "$url" "$checksum_tmp"

    # Support either a single-line `<hash>  <file>` format or a sums file containing many entries.
    if grep -qE "^[a-f0-9]{64}[[:space:]]+\\*?${base_name}\$" "$checksum_tmp"; then
        (cd "$(dirname "$file_path")" && grep -E "^[a-f0-9]{64}[[:space:]]+\\*?${base_name}\$" "$checksum_tmp" | sha256sum -c -) \
            || { msg_error "SHA256 verification failed"; exit 1; }
        msg_ok "SHA256 verified"
        rm -f "$checksum_tmp"
        return 0
    elif grep -qE "^[a-f0-9]{64}[[:space:]]+\\*?-" "$checksum_tmp"; then
        # Not expected, but keep a generic path.
        msg_warn "Checksum file format not recognized for ${base_name}"
    else
        msg_warn "No checksum entry found for ${base_name}"
    fi

    rm -f "$checksum_tmp"
    if confirm "Continue without checksum verification for ${base_name}?"; then
        return 0
    fi
    msg_error "Aborted"
    exit 1
}

reload_udev_rules() {
    msg_info "Reloading udev rules"
    if ! sudo udevadm control --reload-rules || ! sudo udevadm trigger; then
        msg_error "Failed to reload udev rules; you may be able to restart your computer instead"
        return 0
    fi
    msg_ok "Reloaded udev rules"
}

install_flatpak() {
    msg_info "Installing ${FLATHUB_APP_ID} from Flathub"
    if confirm "Install RiverDeck system-wide? (No = user install)"; then
        if ! flatpak remote-list | grep -q flathub; then
            msg_error "Flathub remote not found; please add Flathub before running this script"
            exit 1
        fi
        flatpak install flathub "${FLATHUB_APP_ID}"
    else
        if ! flatpak remote-list --user | grep -q flathub; then
            msg_error "Flathub remote not found; please add Flathub before running this script or try installing system-wide"
            exit 1
        fi
        flatpak install --user flathub "${FLATHUB_APP_ID}"
    fi
    msg_ok "Installed ${FLATHUB_APP_ID} from Flathub"

    msg_info "Installing udev rules"
    tmp_rules="$(mktemp --suffix=.rules)"
    local tag
    tag="$(github_latest_tag || true)"
    if [ -n "${tag}" ]; then
        download_with_retry "${UDEV_RULES_URL_BASE}/${tag}/packaging/linux/40-streamdeck.rules" "$tmp_rules"
    else
        # Fallback to main if tag discovery fails.
        download_with_retry "${UDEV_RULES_URL_BASE}/main/packaging/linux/40-streamdeck.rules" "$tmp_rules"
    fi
    sudo mv "$tmp_rules" /etc/udev/rules.d/40-streamdeck.rules
    sudo chmod 644 /etc/udev/rules.d/40-streamdeck.rules
    msg_ok "Installed udev rules"

    reload_udev_rules
}

install_deb() {
    local dl tmpd tmpf
    dl=$(fetch_latest_asset "deb")
    msg_info "Downloading ${dl##*/}"
    tmpd="$(mktemp -d)"
    tmpf="${tmpd}/${dl##*/}"
    download_with_retry "$dl" "$tmpf"
    msg_ok "Downloaded ${dl##*/}"
    verify_sha256_if_possible "$tmpf"

    msg_info "Installing .deb package"
    sudo apt-get install -y --fix-broken "$tmpf"
    rm -rf "$tmpd"
    msg_ok "Installed .deb package"

    reload_udev_rules
}

install_rpm() {
    local dl tmpd tmpf
    dl=$(fetch_latest_asset "rpm")
    msg_info "Downloading ${dl##*/}"
    tmpd="$(mktemp -d)"
    tmpf="${tmpd}/${dl##*/}"
    download_with_retry "$dl" "$tmpf"
    msg_ok "Downloaded ${dl##*/}"
    verify_sha256_if_possible "$tmpf"

    msg_info "Installing .rpm package"
    msg_warn "For best security, prefer installing signed packages from your distro repos / AUR / Flathub."
    if ! confirm "Proceed to install a locally downloaded RPM file?"; then
        msg_error "Installation aborted"
        exit 1
    fi
    if has_cmd zypper; then
        if confirm "Allow unsigned RPM install if required?"; then
            sudo zypper install -y --allow-unsigned-rpm "$tmpf"
        else
            sudo zypper install -y "$tmpf"
        fi
    elif has_cmd dnf; then
        if confirm "Disable GPG checks if required?"; then
            sudo dnf install -y --nogpgcheck "$tmpf"
        else
            sudo dnf install -y "$tmpf"
        fi
    else
        if confirm "Disable signature checks if required?"; then
            sudo rpm -i --nosignature "$tmpf"
        else
            sudo rpm -i "$tmpf"
        fi
    fi
    rm -rf "$tmpd"
    msg_ok "Installed .rpm package"

    reload_udev_rules
}

install_aur() {
    msg_info "Installing from AUR"
    msg_info "${BOLD}This script will attempt to use yay, paru, aura, pikaur, or trizen, in that order"
    confirm "If you use another AUR helper, you should install RiverDeck manually. Continue?"

    if has_cmd yay; then
        yay -Sy riverdeck
    elif has_cmd paru; then
        paru -Sy riverdeck
    elif has_cmd aura; then
        aura -Ak riverdeck
    elif has_cmd pikaur; then
        pikaur -Sy riverdeck
    elif has_cmd trizen; then
        trizen -Sy riverdeck
    else
        msg_error "No AUR helper found; install yay, paru, aura, pikaur, or trizen, or install manually"
        return 1
    fi
    msg_ok "Installed from AUR"

    reload_udev_rules
}

install_wine_if_needed() {
    if has_cmd wine; then
        msg_info "Wine already installed"
        return
    fi

    if confirm "Wine is required for many plugins. Install Wine now?"; then
        msg_info "Installing Wine"
        case "$PKG_FAMILY" in
        debian)
            sudo apt-get update && sudo apt-get install wine
            ;;
        rpm | ublue)
            if has_cmd rpm-ostree; then
                sudo rpm-ostree install wine wine-mono
            elif has_cmd zypper; then
                sudo zypper install wine wine-mono
            elif has_cmd dnf; then
                sudo dnf install wine wine-mono
            else
                msg_error "No supported package manager found to install Wine; please install it manually"
            fi
            ;;
        arch)
            sudo pacman -Sy wine wine-mono
            ;;
        *)
            msg_error "No supported package manager found to install Wine; please install it manually"
            ;;
        esac
        msg_ok "Installed Wine"
    else
        msg_warn "Not installing Wine; many plugins may not function"
    fi
}

PKG_FAMILY="$(detect_family)"
msg_info "Detected ${PKG_FAMILY} package family"

case "$PKG_FAMILY" in
debian)
    install_deb
    ;;
rpm)
    install_rpm
    ;;
arch)
    install_aur
    ;;
ublue)
    install_flatpak
    ;;
unknown)
    if has_cmd flatpak; then
        msg_warn "No native package method found"
        msg_info "You can continue by installing with Flatpak; if you experience issues, manually install RiverDeck natively"
        if confirm "Install with Flatpak?"; then
            install_flatpak
        else
            msg_error "Installation aborted"
            exit 1
        fi
    else
        msg_error "No usable installation method found; please install RiverDeck manually"
        exit 1
    fi
    ;;
esac

install_wine_if_needed

msg_ok "Installation complete!"
echo -e "${YELLOW}If you enjoy RiverDeck, please consider starring the project on GitHub: ${BLUE}https://github.com/${GITHUB_REPO}${RESET}"

if confirm "Launch RiverDeck now?"; then
    if [[ "$PKG_FAMILY" == "ublue" ]] || { [[ "$PKG_FAMILY" == "unknown" ]] && has_cmd flatpak; }; then
        flatpak run "${FLATHUB_APP_ID}" &
        msg_ok "Launched RiverDeck using Flatpak"
    else
        if [ -x /bin/riverdeck ]; then
            /bin/riverdeck &
            msg_ok "Launched RiverDeck from /bin"
        elif has_cmd riverdeck; then
            riverdeck &
            msg_ok "Launched RiverDeck from PATH"
        else
            msg_warn "RiverDeck executable not found"
        fi
    fi
fi


