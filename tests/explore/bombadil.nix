# Bombadil's static release binary, by hash: its source build brings its own crane, rust-overlay and ghostty.
{
  fetchurl,
  runCommand,
}:
runCommand "bombadil-0.7.10" {} ''
  install -Dm755 ${fetchurl {
    url = "https://github.com/antithesishq/bombadil/releases/download/v0.7.10/bombadil-x86_64-linux";
    hash = "sha256-byZkyn1AZwF7Tvsm21wp1tz+zceBMflpidM28SQgzBs=";
  }} $out/bin/bombadil
''
