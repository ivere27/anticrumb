#!/bin/sh
# Install a verified release into a user-owned directory. No build tools needed.
set -eu
umask 077

fail() { printf 'anticrumb: %s\n' "$*" >&2; exit 1; }
for dependency in curl tar awk mktemp; do
    command -v "$dependency" >/dev/null 2>&1 || fail "Install $dependency and try again."
done
if command -v sha256sum >/dev/null 2>&1; then
    checksum() { sha256sum "$1" | awk '{print $1}'; }
elif command -v shasum >/dev/null 2>&1; then
    checksum() { shasum -a 256 "$1" | awk '{print $1}'; }
else
    fail 'A SHA-256 tool is required (sha256sum or shasum).'
fi

case "$(uname -s)" in
    Linux) platform=unknown-linux-gnu ;;
    Darwin) platform=apple-darwin ;;
    *) fail 'Supported systems: Linux, macOS, and Windows (use install.ps1).' ;;
esac
case "$(uname -m)" in
    x86_64|amd64) architecture=x86_64 ;;
    aarch64|arm64) architecture=aarch64 ;;
    *) fail 'Supported CPU architectures: x86_64 and ARM64.' ;;
esac
target="$architecture-$platform"
scratch=$(mktemp -d "${TMPDIR:-/tmp}/anticrumb-install.XXXXXXXX")
trap 'rm -rf "$scratch"' 0
trap 'exit 1' 1 2 15
fetch() {
    curl --fail --silent --show-error --location --proto '=https' --tlsv1.2 \
        --proto-redir '=https' --max-filesize "${3:-536870912}" --retry 3 "$1" -o "$2"
}
requested=${ANTICRUMB_VERSION:-}
requested=${requested#v}
api='https://api.github.com/repos/ivere27/anticrumb/releases/latest'
if [ -n "$requested" ]; then
    printf '%s\n' "$requested" | awk '/^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/ {ok=1} END {exit !ok}' ||
        fail 'ANTICRUMB_VERSION must be a stable version such as 0.1.0.'
    api="https://api.github.com/repos/ivere27/anticrumb/releases/tags/v$requested"
fi
status=$(curl --silent --show-error --location --proto '=https' --proto-redir '=https' --tlsv1.2 \
    --retry 3 --max-filesize 1048576 --header 'Accept: application/vnd.github+json' \
    --header 'X-GitHub-Api-Version: 2026-03-10' --write-out '%{http_code}' \
    "$api" -o "$scratch/release.json") || fail 'Cannot contact GitHub releases; try again later.'
case "$status" in
    200) ;;
    403|429) fail 'GitHub API access was denied or rate-limited. Wait and retry, or download from https://github.com/ivere27/anticrumb/releases.' ;;
    404) fail 'No stable release is available yet. See https://github.com/ivere27/anticrumb/releases.' ;;
    *) fail "Cannot read GitHub release metadata (HTTP $status). Try again later." ;;
esac
archive="anticrumb-$target.tar.gz"
# Parse JSON as data, including nesting and escaped strings; never evaluate it.
# Only direct release fields and direct asset fields can select a download.
LC_ALL=C awk -v name="$archive" -v requested="$requested" '
function invalid(reason) { failed=1; print "anticrumb: Invalid GitHub release metadata: " reason > "/dev/stderr"; exit 1 }
function space() { while (substr(json,pos,1) ~ /^[ \t\r\n]$/) pos++ }
function hex4(  s,i,n,h) {
    s=substr(json,pos,4); if (length(s)!=4 || s ~ /[^0-9a-fA-F]/) invalid("invalid Unicode escape")
    n=0; for(i=1;i<=4;i++) { h=index("0123456789abcdef",tolower(substr(s,i,1)))-1; n=n*16+h }
    pos+=4; return n
}
function utf8(n) {
    if(n<128) return sprintf("%c",n)
    if(n<2048) return sprintf("%c%c",192+int(n/64),128+n%64)
    if(n<65536) return sprintf("%c%c%c",224+int(n/4096),128+int(n/64)%64,128+n%64)
    return sprintf("%c%c%c%c",240+int(n/262144),128+int(n/4096)%64,128+int(n/64)%64,128+n%64)
}
function string(keep,  out,c,n,low) {
    if(substr(json,pos++,1)!="\"") invalid("expected string")
    out=""
    while(pos<=length(json)) {
        c=substr(json,pos++,1)
        if(c=="\"") return out
        if(c ~ /[[:cntrl:]]/) invalid("unescaped control character")
        if(c=="\\") {
            c=substr(json,pos++,1)
            if(c=="u") {
                n=hex4()
                if(n>=55296 && n<=56319) {
                    if(substr(json,pos,2)!="\\u") invalid("missing Unicode surrogate")
                    pos+=2; low=hex4(); if(low<56320 || low>57343) invalid("invalid Unicode surrogate")
                    n=65536+(n-55296)*1024+low-56320
                } else if(n>=56320 && n<=57343) invalid("invalid Unicode surrogate")
                c=utf8(n)
            } else if(c=="b") c="\b"
            else if(c=="f") c="\f"
            else if(c=="n") c="\n"
            else if(c=="r") c="\r"
            else if(c=="t") c="\t"
            else if(c!="\"" && c!="\\" && c!="/") invalid("invalid string escape")
        }
        if(keep) out=out c
    }
    invalid("unterminated string")
}
function wanted(mode,key) {
    return (mode=="release" && (key=="tag_name" || key=="draft" || key=="prerelease")) ||
           (mode=="asset" && (key=="name" || key=="browser_download_url" || key=="digest" || key=="size" || key=="state"))
}
function value(mode,depth,  c,id,key,child,token) {
    if(depth>64) invalid("JSON nesting exceeds limit")
    space(); c=substr(json,pos,1)
    if(mode=="release" && c!="{") invalid("expected release object")
    if(mode=="assets" && c!="[") invalid("expected asset array")
    if(mode=="asset" && c!="{") invalid("expected asset object")
    if(c=="{") {
        pos++; id=++objects; space()
        if(substr(json,pos,1)!="}") while(1) {
            space(); key=string(1)
            if(seen[id,key]++) invalid("duplicate object field")
            space(); if(substr(json,pos++,1)!=":") invalid("expected colon")
            child=wanted(mode,key)?"scalar":"ignore"
            if(mode=="release" && key=="assets") child="assets"
            value(child,depth+1)
            if(wanted(mode,key)) { field[id,key]=scalar; fieldtype[id,key]=kind }
            if(mode=="release" && key=="assets") haveassets=1
            space(); c=substr(json,pos++,1)
            if(c=="}") break
            if(c!=",") invalid("expected object separator")
        } else pos++
        if(mode=="release") release=id
        if(mode=="asset" && fieldtype[id,"name"]=="string" && field[id,"name"]==name) { matches++; selected=id }
        kind="object"; scalar=""; return
    }
    if(c=="[") {
        pos++; space()
        if(substr(json,pos,1)!="]") while(1) {
            value(mode=="assets"?"asset":"ignore",depth+1)
            space(); c=substr(json,pos++,1)
            if(c=="]") break
            if(c!=",") invalid("expected array separator")
        } else pos++
        kind="array"; scalar=""; return
    }
    if(c=="\"") { scalar=string(mode=="scalar"); kind="string"; return }
    if(match(substr(json,pos),/^-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?/)) {
        scalar=substr(json,pos,RLENGTH); pos+=RLENGTH; kind="number"; return
    }
    token=substr(json,pos,5)
    if(substr(token,1,4)=="true") { pos+=4; scalar="true"; kind="boolean"; return }
    if(token=="false") { pos+=5; scalar="false"; kind="boolean"; return }
    if(substr(token,1,4)=="null") { pos+=4; scalar=""; kind="null"; return }
    invalid("unexpected JSON value")
}
{ json=json $0 "\n"; if(length(json)>1048576) invalid("response exceeds size limit") }
END {
    if(failed) exit 1
    pos=1; value("release",0); space(); if(pos<=length(json)) invalid("trailing JSON data")
    tag=field[release,"tag_name"]
    if(fieldtype[release,"tag_name"]!="string" || tag !~ /^v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$/) invalid("expected stable vX.Y.Z tag")
    if(requested!="" && tag!="v" requested) invalid("requested tag does not match release")
    if(fieldtype[release,"draft"]!="boolean" || field[release,"draft"]!="false" ||
       fieldtype[release,"prerelease"]!="boolean" || field[release,"prerelease"]!="false") invalid("release is not published and stable")
    if(!haveassets || matches!=1) invalid("expected one package for this platform")
    url="https://github.com/ivere27/anticrumb/releases/download/" tag "/" name
    digest=field[selected,"digest"]; size=field[selected,"size"]
    if(fieldtype[selected,"browser_download_url"]!="string" || field[selected,"browser_download_url"]!=url ||
       fieldtype[selected,"state"]!="string" || field[selected,"state"]!="uploaded") invalid("unexpected package URL or upload state")
    if(fieldtype[selected,"digest"]!="string" || length(digest)!=71 || substr(digest,1,7)!="sha256:" || substr(digest,8) ~ /[^0-9a-fA-F]/) invalid("missing or invalid SHA-256 asset digest")
    if(fieldtype[selected,"size"]!="number" || size !~ /^[1-9][0-9]*$/ || size+0>536870912) invalid("invalid package size")
    print substr(tag,2); print url; print tolower(substr(digest,8)); print size
}' "$scratch/release.json" > "$scratch/package-info" || fail 'Release validation failed; nothing was installed.'
{ IFS= read -r version; IFS= read -r download_url; IFS= read -r expected; IFS= read -r expected_size; } < "$scratch/package-info"
printf 'Downloading anticrumb %s for %s…\n' "$version" "$target"
fetch "$download_url" "$scratch/package.tar.gz" "$expected_size" || fail 'Download failed; nothing was installed.'
[ "$(wc -c < "$scratch/package.tar.gz" | tr -d ' ')" = "$expected_size" ] || fail 'Package size mismatch; nothing was installed.'
[ "$(checksum "$scratch/package.tar.gz")" = "$expected" ] || fail 'SHA-256 mismatch; nothing was installed.'

# Stream only exact regular-file entries; never extract archive paths onto disk.
extract_file() {
    entry="anticrumb-$target/$1"
    tar -tvzf "$scratch/package.tar.gz" "$entry" > "$scratch/listing" 2>/dev/null ||
        fail "Package is missing $1."
    awk 'NR != 1 || substr($1,1,1) != "-" {bad=1} END {exit bad || NR != 1}' "$scratch/listing" ||
        fail "Package entry $1 is not one regular file."
    tar -xOzf "$scratch/package.tar.gz" "$entry" > "$scratch/$1" || fail "Cannot read $1."
    [ -s "$scratch/$1" ] || fail "Package entry $1 is empty."
}
extract_file anticrumb

install_dir=${ANTICRUMB_INSTALL_DIR:-"$HOME/.local/bin"}
mkdir -p "$install_dir" || fail "Cannot create $install_dir. Choose a user-writable ANTICRUMB_INSTALL_DIR."
install_file() {
    [ ! -d "$install_dir/$1" ] || fail "Destination $install_dir/$1 is a directory."
    staged=$(mktemp "$install_dir/.anticrumb.XXXXXXXX")
    if ! cat "$scratch/$1" > "$staged" || ! chmod "$2" "$staged" || ! mv -f "$staged" "$install_dir/$1"; then
        rm -f "$staged"
        fail "Cannot install $1 into $install_dir."
    fi
}
install_file anticrumb 755
printf 'Installed anticrumb %s to %s/anticrumb\n' "$version" "$install_dir"

case ":${PATH:-}:" in
    *":$install_dir:"*) ;;
    *)
        if [ "$install_dir" = "$HOME/.local/bin" ]; then
            case "${SHELL:-}" in
                */zsh) profile="$HOME/.zshrc" ;;
                */bash)
                    if [ "$platform" = apple-darwin ]; then profile="$HOME/.bash_profile"; else profile="$HOME/.bashrc"; fi
                    ;;
                *) profile="$HOME/.profile" ;;
            esac
            path_line='export PATH="$HOME/.local/bin:$PATH"'
            if ! grep -Fqx "$path_line" "$profile" 2>/dev/null; then
                printf '\n# anticrumb\n%s\n' "$path_line" >> "$profile" ||
                    fail "Installed successfully, but could not update $profile. Add $install_dir to PATH."
            fi
            printf 'Restart your terminal, or run: %s\n' "$path_line"
        else
            printf 'Add %s to your PATH.\n' "$install_dir"
        fi
        ;;
esac
printf 'Run: anticrumb\nFirst use: /browser login. Models load automatically after sign-in.\n'
command -v rg >/dev/null 2>&1 || printf 'Local coding tools also need ripgrep (rg).\n'
