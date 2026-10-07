{
  description = "oneloop - a local-first coding agent";

  inputs = {
    # One nixpkgs instance for the whole flake. The toolchain and llama.cpp
    # then build against the same package set, and a single `nix flake update
    # llama-cpp` moves both.
    nixpkgs.follows = "llama-cpp/nixpkgs";
    rust-overlay.url = "github:oxalica/rust-overlay";
    # rust-overlay builds its overlay against its own nixpkgs; point it at
    # ours so the flake resolves to a single instance.
    rust-overlay.inputs.nixpkgs.follows = "nixpkgs";
    flake-utils.url = "github:numtide/flake-utils";

    # llama.cpp ships several builds a day. The lock file pins the exact
    # server used by `ols`; update it deliberately with
    # `nix flake update llama-cpp`.
    llama-cpp.url = "github:ggml-org/llama.cpp";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, llama-cpp, ... }:
    # Linux only: `ols` runs llama-server, which needs a Vulkan backend, and
    # nixpkgs has begun dropping x86_64-darwin support.
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        # Follows the overlay for whatever nixpkgs this flake locks, so the
        # toolchain can never be built against a different nixpkgs.
        overlays = [ rust-overlay.overlays.default ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        lib = pkgs.lib;

        # The single source of truth for the Rust version.
        rustChannel =
          (builtins.fromTOML (builtins.readFile ./rust-toolchain.toml)).toolchain.channel;

        # The channel lives in `rust-toolchain.toml` so that rustup outside
        # Nix and the development shell can never disagree. Its components
        # are already in the `default` profile; `rust-src` is not, and
        # `RUST_SRC_PATH` below depends on it.
        rustToolchain = pkgs.rust-bin.stable.${rustChannel}.default.override {
          extensions = [ "rust-src" ];
        };

        # Provide both backends; ols picks at runtime via ONELOOP_BACKEND
        llamaServerVulkan = llama-cpp.packages.${system}.vulkan;
        llamaServerRocm = lib.optionalAttrs (system == "x86_64-linux")
          llama-cpp.packages.${system}.rocm;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            pkg-config
            openssl
            git

            # `./check-updates` compares the pinned llama.cpp revision and
            # model against upstream, and needs both of these.
            curl
            jq
          ];

          shellHook = ''
            export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
            export CARGO_TARGET_DIR="target"

            if [ -z "$${ONELOOP_QUIET:-}" ]; then
              echo "oneloop development environment"
              echo "=============================="
              echo "Rust: $(rustc --version)"
              echo "Cargo: $(cargo --version)"
              echo ""
              echo "Commands:"
              echo "  cargo check"
              echo "  cargo test"
              echo "  cargo run"
              echo ""
              echo "Backends:"
              echo "  ols qwen-coder-7b         # Vulkan (default)"
              echo "  ONELOOP_BACKEND=rocm ols qwen-coder-7b  # ROCm for AMD GPU"
              echo ""
            fi
          '';
        };

        # OneLoop talks to inference over HTTP, so building llama.cpp must not
        # gate the ordinary Rust development shell.
        packages = {
          llama-server-vulkan = llamaServerVulkan;
          llama-server-rocm = llamaServerRocm;
        };
      }
    );
}
