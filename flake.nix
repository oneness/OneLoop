{
  description = "oneloop - a local-first coding agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";

    # llama.cpp ships several builds a day. The lock file pins the exact
    # server used by `ols`; update it deliberately with
    # `nix flake update llama-cpp`.
    llama-cpp.url = "github:ggml-org/llama.cpp";
  };

  outputs = { nixpkgs, rust-overlay, flake-utils, llama-cpp, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable."1.95.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        # Vulkan is the wrong backend on Darwin, and evaluating llama.cpp's
        # x86_64-darwin package currently fails outright.
        hasLlama = pkgs.stdenv.hostPlatform.isLinux
          && llama-cpp.packages ? ${system}
          && llama-cpp.packages.${system} ? vulkan;

        # The flake supplies the pinned executable. Model selection and all
        # llama-server policy belong in `ols`, where they are easy to inspect
        # and change without rebuilding a generated shell application.
        llamaServer = llama-cpp.packages.${system}.vulkan;
      in
      {
        devShells.default = pkgs.mkShell {
          packages = with pkgs; [
            rustToolchain
            pkg-config
            openssl
            git
          ];

          shellHook = ''
            export RUST_SRC_PATH="${rustToolchain}/lib/rustlib/src/rust/library"
            export CARGO_TARGET_DIR="target"

            if [ -z "''${ONELOOP_QUIET:-}" ]; then
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
            fi
          '';
        };

        # OneLoop talks to inference over HTTP, so building llama.cpp must not
        # gate the ordinary Rust development shell.
        packages = pkgs.lib.optionalAttrs hasLlama {
          llama-server = llamaServer;
        };
      }
    );
}
