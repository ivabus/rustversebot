{
  description = "rustversebot Cargo workspace and aarch64-darwin package";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";
    attic = {
      url = "github:zhaofengli/attic";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    { nixpkgs, attic, ... }:
    let
      system = "aarch64-darwin";
      pkgs = import nixpkgs { inherit system; };

      rustversebot = pkgs.rustPlatform.buildRustPackage {
        pname = "rustversebot";
        version = "0.1.0";
        src = pkgs.lib.fileset.toSource {
          root = ./.;
          fileset = pkgs.lib.fileset.unions [
            ./Cargo.lock
            ./Cargo.toml
            ./config.toml
            ./crates
          ];
        };

        cargoLock.lockFile = ./Cargo.lock;
        cargoBuildFlags = [
          "--package"
          "rustversebot"
        ];
        cargoTestFlags = [
          "--package"
          "rustversebot"
        ];

        nativeBuildInputs = [
          pkgs.cmake
          pkgs.makeWrapper
          pkgs.pkg-config
        ];
        buildInputs = [
          pkgs.libiconv
          pkgs.openssl
        ];

        postInstall = ''
          install -Dm444 config.toml "$out/share/rustversebot/config.toml"
          wrapProgram "$out/bin/rustversebot" \
            --set-default BOT_CONFIG_PATH "$out/share/rustversebot/config.toml"
        '';

        meta = {
          description = "Telegram bot for tracking Zenless Zone Zero endgame results";
          mainProgram = "rustversebot";
          platforms = [ system ];
        };
      };
    in
    {
      packages.${system} = {
        default = rustversebot;
        inherit rustversebot;
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${rustversebot}/bin/rustversebot";
        };
        attic = {
          type = "app";
          program = "${attic.packages.${system}.attic-client}/bin/attic";
        };
      };

      checks.${system}.default = rustversebot;

      devShells.${system}.default = pkgs.mkShell {
        inputsFrom = [ rustversebot ];
        packages = [
          pkgs.cargo
          pkgs.rustc
          pkgs.rustfmt
          pkgs.clippy
          attic.packages.${system}.attic-client
        ];
      };
    };
}
