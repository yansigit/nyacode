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
          nyacode = rustPlatform.buildRustPackage {
            pname = "nyacode";
            inherit version;
            src = preparedEngine + "/codex-rs";
            cargoLock = {
              lockFile = preparedEngine + "/codex-rs/Cargo.lock";
              # Git revisions remain fixed by the committed assembled lock.
              allowBuiltinFetchGit = true;
            };
            cargoBuildFlags = [
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
              pkgs.llvmPackages.clang
              pkgs.llvmPackages.libclang.lib
            ];
            buildInputs = [ pkgs.openssl ]
              ++ pkgs.lib.optionals pkgs.stdenv.isLinux [ pkgs.libcap ];
            env = {
              LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";
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
