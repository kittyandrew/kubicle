# The server, the WebAssembly client, the container image that carries both, and the fmt and clippy checks.
# crane builds the dependencies once per target into their own derivations, cached until Cargo.lock or a
# Cargo.toml's dependency settings change, so a source change recompiles only kubicle's own crates.
{
  binaryen,
  brotli,
  craneLib,
  dockerTools,
  fenix,
  lib,
  wasm-bindgen-cli,
  profile ? "release", # `release` ships; the suites test `fast` (Cargo.toml), the same optimisation built in parallel
}: let
  toolchain = fenix.combine [
    fenix.stable.rustc
    fenix.stable.cargo
    fenix.stable.clippy
    fenix.latest.rustfmt # nightly, for rustfmt.toml's unstable options; the compiler stays stable
    fenix.stable.rust-std
    fenix.targets.wasm32-unknown-unknown.stable.rust-std # the client
  ];
  # The same cargo and rustfmt without the compiler, for `cargo fmt` in the lint shell (tests/lint.sh).
  fmtToolchain = fenix.combine [fenix.stable.cargo fenix.latest.rustfmt];
  crane = craneLib.overrideToolchain (_: toolchain);

  src = lib.fileset.toSource {
    root = ../.;
    fileset = lib.fileset.unions [../Cargo.toml ../Cargo.lock ../rustfmt.toml ../crates];
  };

  common = {
    inherit src;
    pname = "kubicle";
    version = "0.1.0";
    strictDeps = true;
    doCheck = false; # no unit tests: the suites are separate flake checks (docs/design.md, Testing)
    CARGO_PROFILE = profile;
  };
  serverArgs = common // {cargoExtraArgs = "-p kubicle-server";};
  webArgs =
    common
    // {
      cargoExtraArgs = "-p kubicle-web";
      CARGO_BUILD_TARGET = "wasm32-unknown-unknown";
      # Whole-program LTO for the client alone, 11 KB smaller in brotli and no slower (docs/design.md,
      # Deployment); the server keeps release's thin LTO.
      CARGO_PROFILE_RELEASE_LTO = "fat";
    };
  serverDeps = crane.buildDepsOnly serverArgs;
  webDeps = crane.buildDepsOnly webArgs;

  server = crane.buildPackage (serverArgs
    // {
      pname = "kubicle-server";
      cargoArtifacts = serverDeps;
      meta.mainProgram = "kubicle-server";
    });

  web = crane.buildPackage (webArgs
    // {
      pname = "kubicle-web";
      cargoArtifacts = webDeps;
      nativeBuildInputs = [wasm-bindgen-cli binaryen brotli];
      installPhaseCommand = ''
        mkdir -p $out
        wasm-bindgen --target web --no-typescript --out-name kubicle_web --out-dir $out \
          target/wasm32-unknown-unknown/${profile}/kubicle-web.wasm
        wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int -o $out/kubicle_web_bg.wasm $out/kubicle_web_bg.wasm
        cp -r crates/kubicle-web/static/. $out/
        cd $out
        # Each file the page loads is named after its content, so a browser can keep it for good and a deploy
        # still reaches it (crates/kubicle-server/src/assets.rs); boot.js and index.html name the new files.
        hashed() { mv "$1" "''${1%.*}.$(sha256sum "$1" | cut -c1-16).''${1##*.}"; ls "''${1%.*}".????????????????."''${1##*.}"; }
        wasm=$(hashed kubicle_web_bg.wasm)
        js=$(hashed kubicle_web.js)
        substituteInPlace boot.js --replace-fail ./kubicle_web.js "./$js" --replace-fail "init()" "init({ module_or_path: \"$wasm\" })" \
          --replace-fail 'kubicleBuild = "dev"' "kubicleBuild = \"$wasm\""
        boot=$(hashed boot.js)
        css=$(hashed base.css)
        substituteInPlace index.html --replace-fail '"base.css"' "\"$css\"" --replace-fail '"kubicle_web.js"' "\"$js\"" \
          --replace-fail '"kubicle_web_bg.wasm"' "\"$wasm\"" --replace-fail '"boot.js"' "\"$boot\""
        # Themes keep their names, which are their ids, and carry the hash as `?v=`: themes::info's version.
        for theme in latte frappe; do
          substituteInPlace index.html --replace-fail "\"themes/$theme.css\"" \
            "\"themes/$theme.css?v=$(sha256sum themes/$theme.css | cut -c1-16)\""
        done
        # Served in place of the plain file to every browser that accepts brotli (http.rs, `precompressed_br`).
        find . -type f ! -name '*.png' -exec brotli --best --keep {} +
      '';
    });

  image = dockerTools.buildLayeredImage {
    name = "kubicle";
    tag = "latest";
    contents = [dockerTools.caCertificates];
    config = {
      Entrypoint = [(lib.getExe server)];
      Env = ["KUBICLE_WEB_DIR=${web}"];
      ExposedPorts."80/tcp" = {};
      User = "65534:65534"; # nobody: kubicle needs no privileges
      # Exec form: `docker run --health-cmd` wraps its command in `/bin/sh -c`, and the image has no shell.
      # `--health-interval` and the other `--health-*` flags still apply over it.
      Healthcheck.Test = ["CMD" (lib.getExe server) "--health"];
    };
  };

  checks = {
    fmt = crane.cargoFmt {inherit (common) src pname version;}; # else crane warns the workspace root has no name
    clippy-server = crane.cargoClippy (serverArgs
      // {
        cargoArtifacts = serverDeps;
        cargoClippyExtraArgs = "-p kubicle-api -p kubicle-imap -p kubicle-mail -p kubicle-mailbox -- -D warnings";
      });
    clippy-web = crane.cargoClippy (webArgs
      // {
        cargoArtifacts = webDeps;
        cargoClippyExtraArgs = "-p kubicle-ui -p kubicle-browser -- -D warnings";
      });
  };
in {
  inherit toolchain fmtToolchain server web image checks;
}
