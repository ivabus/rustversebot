{
  description = "rustversebot Cargo workspace and aarch64-darwin package";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    { crane, nixpkgs, ... }:
    let
      system = "aarch64-darwin";
      pkgs = import nixpkgs { inherit system; };
      craneLib = crane.mkLib pkgs;
      cpWithoutPreservedMode = pkgs.writeShellScriptBin "cp" ''
        exec ${pkgs.coreutils}/bin/cp --no-preserve=mode,ownership "$@"
      '';

      src = pkgs.lib.fileset.toSource {
        root = ./.;
        fileset = pkgs.lib.fileset.unions [
          ./Cargo.lock
          ./Cargo.toml
          ./crates
        ];
      };

      commonArgs = {
        pname = "rustversebot";
        version = "0.1.0";
        inherit src;
        strictDeps = true;
        doCheck = false;
        cargoExtraArgs = "--package rustversebot";

        nativeBuildInputs = [
          pkgs.cmake
          pkgs.makeWrapper
          pkgs.pkg-config
        ];
        buildInputs = [
          pkgs.libiconv
          pkgs.openssl
        ];

        # libsql-ffi copies the same generated binding twice. GNU cp preserves
        # its read-only Nix source mode with `-R`, so make both copies writable.
        preBuild = ''
          export PATH="${cpWithoutPreservedMode}/bin:$PATH"
        '';
      };

      cargoArtifacts = craneLib.buildDepsOnly (commonArgs // {
        buildPhaseCargoCommand = "cargo build --release --locked ${commonArgs.cargoExtraArgs}";
      });

      rustversebot = craneLib.buildPackage (commonArgs // {
        inherit cargoArtifacts;
        # The libsql-ffi workaround is only needed while producing the
        # dependency artifact. Keeping it here would strip the executable bit
        # when crane installs the finished bot.
        preBuild = "";
        postInstall = ''
          wrapProgram "$out/bin/rustversebot" \
            --prefix PATH : ${pkgs.lib.makeBinPath [ pkgs.typst pkgs.resvg ]}
        '';

        meta = {
          description = "Telegram bot for tracking Zenless Zone Zero endgame results";
          mainProgram = "rustversebot";
          platforms = [ system ];
        };
      });
    in
    {
      packages.${system} = {
        default = rustversebot;
        inherit cargoArtifacts rustversebot;
      };

      apps.${system} = {
        default = {
          type = "app";
          program = "${rustversebot}/bin/rustversebot";
        };
        attic = {
          type = "app";
          program = "${pkgs.attic-client}/bin/attic";
        };
      };

      checks.${system} = {
        default = rustversebot;
      };

      devShells.${system}.default = pkgs.mkShell {
        inputsFrom = [ rustversebot ];
        packages = [
          pkgs.cargo
          pkgs.rustc
          pkgs.rustfmt
          pkgs.typst
          pkgs.resvg
          pkgs.clippy
          pkgs.python3
          pkgs.attic-client
        ];
      };
    };
}
