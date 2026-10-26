{
  description = "kubicle: a webmail for Mailu in Rust, in place of Roundcube";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    fenix.url = "github:nix-community/fenix"; # stable Rust plus the wasm32 target for the client
    fenix.inputs.nixpkgs.follows = "nixpkgs";
    crane.url = "github:ipetkov/crane"; # dependencies built once and cached; only kubicle's crates rebuild
  };

  outputs = {
    nixpkgs,
    fenix,
    crane,
    ...
  }: let
    system = "x86_64-linux";
    pkgs = nixpkgs.legacyPackages.${system};
    build = profile:
      pkgs.callPackage ./nix/package.nix {
        fenix = fenix.packages.${system};
        craneLib = crane.mkLib pkgs;
        inherit profile;
      };
    kubicle = build "release";
    tested = build "fast"; # what the suites run: rebuilds in seconds, not one core's half minute
    # What tests/lint.sh runs, besides cargo fmt
    lintTools = [
      pkgs.alejandra
      pkgs.deadnix
      pkgs.taplo
      pkgs.ruff
      pkgs.mypy
      pkgs.prettier
      pkgs.oxlint
      pkgs.shellcheck
      pkgs.zizmor
    ];
  in {
    packages.${system} = {
      inherit (kubicle) server web image;
      default = kubicle.image;
    };

    checks.${system} =
      kubicle.checks
      // {
        inherit (kubicle) server web;
        # `taplo fmt` as .taplo.toml sets it, over every TOML file
        toml = pkgs.runCommand "kubicle-toml" {nativeBuildInputs = [pkgs.taplo];} ''
          cd ${pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.fileFilter (f: f.hasExt "toml") ./.;
          }}
          taplo fmt --check && touch $out
        '';
        hostile = pkgs.callPackage ./tests/hostile {inherit (tested) server web;};
        explore = pkgs.callPackage ./tests/explore {inherit (tested) server web;};
        vm-full = import ./tests/vm/full.nix {
          inherit pkgs;
          kubicle = tested;
        };
        vm-settings = import ./tests/vm/settings.nix {
          inherit pkgs;
          kubicle = tested;
        };
      }
      // import ./tests/vm/live.nix {
        inherit pkgs;
        kubicle = tested;
      };

    devShells.${system} = {
      default = pkgs.mkShell {
        packages = [kubicle.toolchain pkgs.wasm-bindgen-cli pkgs.binaryen pkgs.sqlite] ++ lintTools;
      };
      # tests/lint.sh alone, for CI (.github/workflows/quality.yml): no compiler, wasm target or C toolchain to fetch
      lint = pkgs.mkShellNoCC {packages = [kubicle.fmtToolchain] ++ lintTools;};
    };

    formatter.${system} = pkgs.alejandra;
  };
}
