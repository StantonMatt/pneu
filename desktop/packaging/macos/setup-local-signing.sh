#!/usr/bin/env bash
# Run once on the Mac; retain this identity to preserve macOS permission grants.
set +x
set -euo pipefail
umask 077
[[ $(uname -s) == Darwin ]] || { echo 'Set up local signing on macOS' >&2; exit 1; }
name='Pneu Local Signing'
keychain="$HOME/Library/Keychains/pneu-signing.keychain-db"
state="$HOME/Library/Application Support/Pneu-signing"
password_file="$state/keychain-password"
cert="$state/certificate.pem"
pending="$state/identity.p12"
key="$state/private-key.pem"
mkdir -p "$state"
[[ ! -L "$state" ]] || { echo 'Refusing a symlink signing directory' >&2; exit 1; }
chmod 700 "$state"
mkdir "$state/setup.lock" 2>/dev/null || { echo "Setup is already running, or an interrupted setup left $state/setup.lock" >&2; exit 1; }
trap 'rmdir "$state/setup.lock"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
for path in "$password_file" "$cert" "$pending" "$key" "$keychain"; do
    [[ ! -L "$path" ]] || { echo "Refusing signing-state symlink: $path" >&2; exit 1; }
done
if [[ ! -f "$password_file" ]]; then
    [[ ! -e "$keychain" && ! -e "$cert" && ! -e "$pending" && ! -e "$key" ]] || {
        echo 'Existing signing state has no password; restore it from backup, do not regenerate the identity' >&2; exit 1;
    }
    openssl rand -hex 32 > "$password_file"
fi
chmod 600 "$password_file"
password=$(cat "$password_file")
[[ -n "$password" ]] || { echo 'Empty keychain password file' >&2; exit 1; }
if [[ ! -f "$cert" ]]; then
    [[ ! -e "$keychain" && ! -e "$pending" && ! -e "$key" ]] || {
        echo 'Incomplete signing state; restore the original certificate, do not regenerate it' >&2; exit 1;
    }
    openssl req -x509 -newkey rsa:2048 -sha256 -nodes -days 7300 \
        -subj "/CN=$name/" -keyout "$key" -out "$cert" \
        -addext 'basicConstraints=critical,CA:FALSE' \
        -addext 'keyUsage=critical,digitalSignature' \
        -addext 'extendedKeyUsage=codeSigning' 2>/dev/null
fi
# Keep the original key until import succeeds, so interrupted setup can resume.
if [[ -f "$key" ]]; then
    p12=(openssl pkcs12 -export)
    if [[ $(openssl version) == 'OpenSSL 3.'* ]]; then p12+=(-legacy); fi
    "${p12[@]}" -inkey "$key" -in "$cert" \
        -name "$name" -passout "file:$password_file" -out "$pending"
fi
if [[ ! -f "$keychain" ]]; then
    [[ -f "$pending" ]] || { echo 'Missing signing keychain; restore it from backup' >&2; exit 1; }
    security create-keychain -p "$password" "$keychain"
fi
security unlock-keychain -p "$password" "$keychain"
security set-keychain-settings "$keychain"
# codesign --keychain restricts identity lookup, but its certificate-chain
# lookup still uses the search list, even for a self-signed leaf certificate.
keychains=("$keychain")
listed=false
while IFS= read -r path; do
    if [[ "$path" == "$keychain" ]]; then listed=true; else keychains+=("$path"); fi
done < <(security list-keychains -d user | sed -n 's/^[[:space:]]*"\(.*\)"$/\1/p')
if [[ "$listed" == false ]]; then
    security list-keychains -d user -s "${keychains[@]}"
fi
identity=$(openssl x509 -in "$cert" -noout -fingerprint -sha1 | sed 's/.*=//;s/://g')
if ! security find-identity -p codesigning "$keychain" | grep -Fq "$identity"; then
    [[ -f "$pending" ]] || { echo 'Original private key is missing; restore the signing keychain from backup' >&2; exit 1; }
    security import "$pending" -k "$keychain" -P "$password" -T /usr/bin/codesign >/dev/null
fi
security set-key-partition-list -S apple-tool:,apple:,codesign: -s -k "$password" "$keychain" >/dev/null
openssl x509 -in "$cert" -noout -checkend 0 >/dev/null
# A local self-signed identity need not be a system-trusted root to sign code.
security find-identity -p codesigning "$keychain" | grep -Fq "$identity" || {
    echo 'Local signing identity is unavailable; original signing material retained' >&2; exit 1;
}
rm -f -- "$pending" "$key"
printf 'Local signing identity: %s (%s)\nKeychain: %s\nPassword file: %s\n' "$name" "$identity" "$keychain" "$password_file"
