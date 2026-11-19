#!/bin/sh
# Run only with the reviewed 4.36.0 release in a fresh disposable directory.
set -eu
src=$(cd "$(dirname "$0")/sources" && pwd -P)
release=${1:?absolute reviewed release}
dest=${2:?absolute fresh output directory}
mkdir -m 700 "$dest"
mkdir "$dest/tmp"
for variant in selected alternate; do
  mkdir "$dest/$variant"
  cp "$src/$variant/QualifiedPlugin.lean" "$dest/$variant/"
  (
    cd "$dest/$variant"
    /usr/bin/env -i TMPDIR="$dest/tmp" HOME="$HOME" PATH="$release/bin:/usr/bin:/bin" "$release/bin/lean" -j1 -o QualifiedPlugin.olean -c QualifiedPlugin.c QualifiedPlugin.lean
    if [ "$variant" = selected ]; then
      /usr/bin/env -i TMPDIR="$dest/tmp" HOME="$HOME" PATH="$release/bin:/usr/bin:/bin" "$release/bin/leanc" -DLEAN_EXPORTING -shared -o libQualifiedPlugin.dylib QualifiedPlugin.c
      /usr/bin/nm -gU libQualifiedPlugin.dylib
      /usr/bin/otool -L libQualifiedPlugin.dylib
    fi
    /usr/bin/shasum -a 256 QualifiedPlugin.*
    if [ "$variant" = selected ]; then /usr/bin/shasum -a 256 libQualifiedPlugin.dylib; fi
  )
done
