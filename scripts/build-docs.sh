#!/bin/sh
# Builds the API reference as static HTML in docs/api/, and the user guide in
# docs/guide/html/. Open their index.html in a browser; no server or internet connection
# is needed.
#
# The library is always documented. The idn-ffmpeg adapter (compressed video, streaming,
# recording, files as sources) is documented too when FFmpeg's development files are
# installed (pkg-config finds libavcodec); otherwise it is left out with a note.
set -eu
cd "$(dirname "$0")/.."
rm -rf target/doc
if pkg-config --exists libavcodec libavformat libswscale libswresample 2>/dev/null; then
    cargo doc --no-deps --lib -p idn-open-protocol -p idn-ffmpeg
    ffmpeg_entry='<li><a href="idn_ffmpeg/index.html">idn-ffmpeg</a>: hardware video encoders and decoders through the system'"'"'s FFmpeg (compressed video), streaming to platforms and recording (gateway), video files as sources (player)</li>'
else
    cargo doc --no-deps --lib -p idn-open-protocol
    ffmpeg_entry='<li>idn-ffmpeg: not built here (FFmpeg development files not found; install libavcodec-dev and friends, then run this again)</li>'
fi
rm -rf docs/api
cp -r target/doc docs/api
# A front page at the top of the folder, linking each crate.
cat > docs/api/index.html <<EOF
<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<title>IDN Open Protocol API</title>
<style>body { font: 16px/1.5 system-ui, sans-serif; max-width: 46em; margin: 2em auto; padding: 0 1em; }</style>
</head>
<body>
<h1>IDN Open Protocol API reference</h1>
<ul>
<li><a href="idn_open_protocol/index.html">idn-open-protocol</a>: the library (outputs, inputs, discovery, encryption, site trust, the IDN extensions)</li>
$ffmpeg_entry
</ul>
</body>
</html>
EOF
# The user guide (G7), for churches' volunteers and technical directors.
cargo run -q -p idn-guide -- docs/guide docs/guide/html
echo "API reference: $(pwd)/docs/api/index.html"
echo "User guide:    $(pwd)/docs/guide/html/index.html"
