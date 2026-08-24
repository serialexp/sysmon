#!/bin/sh
# Download and install the latest sysmon release for this Linux machine.

set -eu

REPO="serialexp/sysmon"
APP_NAME="sysmon"

info() {
    printf '==> %s\n' "$1"
}

error() {
    printf 'error: %s\n' "$1" >&2
    exit 1
}

cleanup() {
    if [ -n "${tmp_dir:-}" ] && [ -d "$tmp_dir" ]; then
        rm -rf "$tmp_dir"
    fi
}
trap cleanup EXIT HUP INT TERM

command -v curl >/dev/null 2>&1 || error "curl is required"
command -v tar >/dev/null 2>&1 || error "tar is required"

[ "$(uname -s)" = "Linux" ] || error "sysmon supports Linux only"
case "$(uname -m)" in
    x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
    aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
    *) error "unsupported architecture: $(uname -m)" ;;
esac

release_json="$(curl --fail --silent --show-error --location \
    -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/${REPO}/releases/latest")" || \
    error "failed to fetch the latest GitHub release"
version="$(printf '%s\n' "$release_json" \
    | grep -m1 '"tag_name"' \
    | cut -d '"' -f 4)"
[ -n "$version" ] || error "latest GitHub release did not contain a tag"
version_number="${version#v}"
archive="${APP_NAME}-${version_number}-${target}.tar.gz"
base_url="https://github.com/${REPO}/releases/download/${version}"

tmp_dir="$(mktemp -d)"
info "Downloading ${APP_NAME} ${version} for ${target}"
curl --fail --silent --show-error --location \
    "${base_url}/${archive}" -o "${tmp_dir}/${archive}"
curl --fail --silent --show-error --location \
    "${base_url}/SHA256SUMS" -o "${tmp_dir}/SHA256SUMS"

if command -v sha256sum >/dev/null 2>&1; then
    expected="$(grep "  ${archive}$" "${tmp_dir}/SHA256SUMS" || true)"
    [ -n "$expected" ] || error "release checksum for ${archive} is missing"
    (cd "$tmp_dir" && printf '%s\n' "$expected" | sha256sum -c -) || \
        error "release checksum verification failed"
else
    error "sha256sum is required to verify the downloaded release"
fi

tar -xzf "${tmp_dir}/${archive}" -C "$tmp_dir"
[ -f "${tmp_dir}/${APP_NAME}" ] || error "release archive contains no ${APP_NAME} binary"

if [ -n "${SYSMON_INSTALL_DIR:-}" ]; then
    bin_dir="$SYSMON_INSTALL_DIR"
elif [ -w /usr/local/bin ]; then
    bin_dir="/usr/local/bin"
else
    bin_dir="${HOME}/.local/bin"
fi
mkdir -p "$bin_dir"

info "Installing to ${bin_dir}/${APP_NAME}"
install -m 755 "${tmp_dir}/${APP_NAME}" "${bin_dir}/${APP_NAME}"

printf '\nInstalled %s %s to %s/%s\n' "$APP_NAME" "$version" "$bin_dir" "$APP_NAME"
if ! command -v "$APP_NAME" >/dev/null 2>&1; then
    printf 'warning: %s is not currently on PATH\n' "$bin_dir" >&2
    printf 'Add it with: export PATH="%s:$PATH"\n' "$bin_dir" >&2
fi
printf 'For full process I/O attribution, optionally run: %s --grant\n' "${bin_dir}/${APP_NAME}"
