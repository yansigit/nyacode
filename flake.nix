{
  description = "Nyacode packages and development shell";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, rust-overlay, ... }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems f;
      engineLock = builtins.fromJSON (builtins.readFile ./engine.lock);
      version = nixpkgs.lib.removeSuffix "\n" (builtins.readFile ./VERSION);
      pkgsFor = system: import nixpkgs {
        inherit system;
        overlays = [ rust-overlay.overlays.default ];
      };
      toolchainFor = pkgs:
        pkgs.rust-bin.stable.${engineLock.toolchain} or
          (throw "Rust ${engineLock.toolchain} is absent from the pinned rust-overlay; explicitly update and review its flake.lock entry.");

      # Preparation is explicit. Nix snapshots the supplied workspace without
      # downloading an engine archive or resolving a new Cargo.lock.
      preparedEngine =
        let
          supplied = builtins.getEnv "NYACODE_ENGINE_ROOT";
          source = builtins.path {
            path = builtins.toPath supplied;
            name = "nyacode-prepared-engine";
            filter = path: type: !(builtins.elem (toString path) [
              "${supplied}/target" "${supplied}/codex-rs/target"
            ]);
          };
          report = builtins.fromJSON (builtins.readFile (source + "/.git/nyacode/inputs.json"));
        in
        if supplied == "" || !nixpkgs.lib.hasPrefix "/" supplied then
          throw "Run python3 scripts/engine.py prepare, then set NYACODE_ENGINE_ROOT to its absolute output path and use nix build --impure."
        else
          assert report.generated_by == "nyacode-engine-v1";
          assert report.revision == engineLock.revision;
          assert report.archive_sha256 == engineLock.archive_sha256;
          assert report.product_version == version;
          assert report.inputs."engine.lock".sha256 == builtins.hashFile "sha256" ./engine.lock;
          assert builtins.hashFile "sha256" (source + "/codex-rs/Cargo.lock")
            == builtins.hashFile "sha256" (./. + "/${engineLock.assembled_lock}");
          source;
    in
    {
      packages = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rust = (toolchainFor pkgs).minimal;
          rustPlatform = pkgs.makeRustPlatform { cargo = rust; rustc = rust; };
          cargoLock = builtins.fromTOML (builtins.readFile (preparedEngine + "/codex-rs/Cargo.lock"));
          v8Packages = builtins.filter (package: package.name == "v8") cargoLock.package;
          v8Version = assert builtins.length v8Packages == 1; (builtins.head v8Packages).version;
          target = {
            x86_64-linux = "x86_64-unknown-linux-gnu";
            aarch64-linux = "aarch64-unknown-linux-gnu";
            x86_64-darwin = "x86_64-apple-darwin";
            aarch64-darwin = "aarch64-apple-darwin";
          }.${system};
          v8Repo = "rusty_v8_${builtins.replaceStrings [ "." ] [ "_" ] v8Version}_${builtins.replaceStrings [ "-" ] [ "_" ] target}";
          # Reuse the assembled Bazel release pins and reject missing or ambiguous assets.
          moduleBlocks = map (block: builtins.head (pkgs.lib.splitString ")" block))
            (pkgs.lib.splitString "http_file(" (builtins.readFile (preparedEngine + "/MODULE.bazel")));
          capture = pattern: text:
            let result = builtins.match pattern text;
            in if result == null then throw "Unexpected pinned V8 http_file declaration" else builtins.head result;
          v8Asset = kind:
            let
              blocks = builtins.filter (block:
                builtins.match ''[[:space:]]*name = "${v8Repo}_${kind}",[^)]*'' block != null
              ) moduleBlocks;
              block = assert builtins.length blocks == 1; builtins.head blocks;
              filename = if kind == "archive"
                then "librusty_v8_ptrcomp_sandbox_release_${target}.a.gz"
                else "src_binding_ptrcomp_sandbox_release_${target}.rs";
              url = "https://github.com/openai/codex/releases/download/rusty-v8-v${v8Version}/${filename}";
            in
            assert capture ''[^)]*downloaded_file_path = "([^"]+)",[^)]*'' block == filename;
            assert capture ''[^)]*urls = [[][[:space:]]*"([^"]+)",[[:space:]]*[]],[[:space:]]*'' block == url;
            pkgs.fetchurl {
              inherit url;
              name = filename;
              sha256 = capture ''[^)]*sha256 = "([0-9a-f]{64})",[^)]*'' block;
            };
          nyacode = rustPlatform.buildRustPackage {
            pname = "nyacode";
            inherit version;
            src = preparedEngine;
            cargoRoot = "codex-rs";
            buildAndTestSubdir = "codex-rs";
            cargoLock = {
              lockFile = preparedEngine + "/codex-rs/Cargo.lock";
              # Git revisions remain fixed by the committed assembled lock.
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [
              "--locked"
              "-p" "codex-cli" "-p" "codex-code-mode-host"
              "--bin" "nyacode" "--bin" "codex" "--bin" "codex-code-mode-host"
            ];
            doCheck = false;
            postInstall = ''
              mkdir -p "$out/share/nyacode"
              cp ${preparedEngine}/.git/nyacode/inputs.json "$out/share/nyacode/engine-inputs.json"
              cp ${./engine.lock} "$out/share/nyacode/engine.lock"
            '';
            nativeBuildInputs = [
              pkgs.cmake
              pkgs.pkg-config
              pkgs.protobuf
              pkgs.llvmPackages.clang
              pkgs.llvmPackages.libclang.lib
            ];
            buildInputs = [ pkgs.openssl ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.libcap ];
            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
              PROTOC = "${pkgs.protobuf}/bin/protoc";
              RUSTY_V8_ARCHIVE = v8Asset "archive";
              RUSTY_V8_SRC_BINDING_PATH = v8Asset "binding";
              CC = "clang";
              CXX = "clang++";
            };
            meta = {
              description = "Nyacode command-line interface";
              license = pkgs.lib.licenses.asl20;
              mainProgram = "nyacode";
            };
          };
        in
        {
          inherit nyacode;
          codex-rs = nyacode;
          default = nyacode;
        }
      );

      devShells = forAllSystems (system:
        let
          pkgs = pkgsFor system;
          rust = (toolchainFor pkgs).default.override {
            extensions = [ "rust-src" "rust-analyzer" ];
          };
        in
        {
          default = pkgs.mkShell {
            buildInputs = [
              rust
              pkgs.gh
              pkgs.python3
              pkgs.git
              pkgs.pkg-config
              pkgs.protobuf
              pkgs.openssl
              pkgs.cmake
              pkgs.llvmPackages.clang
              pkgs.llvmPackages.libclang.lib
            ];
            PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
            LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
            shellHook = ''
              export CC=clang
              export CXX=clang++
            '';
          };
        }
      );
    };
}
