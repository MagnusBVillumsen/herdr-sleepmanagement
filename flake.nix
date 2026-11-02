{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    utils.url = "github:numtide/flake-utils";
  };

  outputs =
    {
      self,
      nixpkgs,
      utils,
    }:
    utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import nixpkgs { inherit system; };
      in
      rec {
        packages.herdr-sleep = pkgs.rustPlatform.buildRustPackage {
          pname = "herdr-sleep";
          version = "0.2.0";
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # A plugin directory for `herdr plugin link`, using the built binary.
          postInstall = ''
            plugin=$out/share/herdr-sleep/plugin
            mkdir -p $plugin/target/release
            sed '/^\[\[build\]\]/,/^$/d' herdr-plugin.toml > $plugin/herdr-plugin.toml
            ln -s $out/bin/herdr-sleep $plugin/target/release/herdr-sleep
          '';
        };
        packages.default = packages.herdr-sleep;

        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo
            rustc
            clippy
            rustfmt
          ];
        };
      }
    );
}
