#!/bin/sh
# Compares one of tergi's lexers with the real one. See README.md.
#
# Usage: dev/reference/run.sh <name>
#
# <name> is a directory next to this script. The script builds its image,
# runs its `prepare` script there to collect the input files, lexes them
# with tergi in the dev image, and runs its `compare` script to check the
# tokens.

set -eu

if [ $# -ne 1 ] || [ ! -f "$(dirname "$0")/$1/Containerfile" ]; then
    echo "usage: $0 <name>" >&2
    echo "names: $(cd "$(dirname "$0")" && ls -d */ | tr -d / | tr '\n' ' ')" >&2
    exit 2
fi
name=$1
here=$(cd "$(dirname "$0")" && pwd)
root=$(cd "$here/../.." && pwd)
podman=${PODMAN:-podman}

# The work directory keeps downloads between runs, so a second run doesn't
# fetch them again. It's outside the project because a corpus can have tens
# of thousands of files.
work=${TERGI_REFERENCE_DIR:-${TMPDIR:-/tmp}/tergi-reference}/$name
mkdir -p "$work"
rm -rf "$work/tokens"

mkdir -p "$root/target"
"$podman" build -t localhost/tergi-dev "$root/dev"
"$podman" build -t "localhost/tergi-reference-$name" "$here/$name"

# Both images see the project and the work directory at the same paths as
# here, so that the paths in files.txt work everywhere. CASES and SEED pass
# through for the checks that generate random texts.
run() {
    image=$1
    shift
    "$podman" run --rm \
        -v "$root:$root:ro" -v "$work:$work" -w "$work" \
        -e CASES -e SEED \
        "$image" "$@"
}

echo "== Preparing the input files"
run "localhost/tergi-reference-$name" "$here/$name/prepare" "$work"

# The named volumes keep the crates and the build between runs. The target
# directory is a volume rather than the project's, which is mounted read
# only here.
echo "== Lexing them with tergi"
"$podman" run --rm \
    -v "$root:$root:ro" -v "$work:$work" -w "$root" \
    -v tergi-cargo-registry:/usr/local/cargo/registry \
    -v tergi-cargo-target:"$root/target" \
    -e LEX_FILES="$work/files.txt" -e LEX_OUT="$work/tokens" \
    localhost/tergi-dev \
    cargo test --release --locked --lib highlight::reference::dump_tokens \
    -- --ignored --exact --quiet

echo "== Comparing"
run "localhost/tergi-reference-$name" "$here/$name/compare" "$work"
