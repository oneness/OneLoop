{
  description = "oneloop - a tiny, extensible coding agent";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";

    # The local inference server, tracked at upstream master.
    #
    # llama.cpp ships several builds a day and fixes land that matter here:
    # the Qwen3 chat parser (PR #26252) went in the same day it was needed,
    # and without it tool calls emitted inside a <think> block are swallowed
    # and the agent silently does nothing. `nix flake update llama-cpp`
    # takes today's build; flake.lock pins the revision, so a bad day is one
    # `git checkout HEAD~1 -- flake.lock` away. Pin deliberately by changing
    # this to a tag, e.g. github:ggml-org/llama.cpp/b10229.
    llama-cpp.url = "github:ggml-org/llama.cpp";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, llama-cpp, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs {
          inherit system overlays;
        };

        rustToolchain = pkgs.rust-bin.stable."1.95.0".default.override {
          extensions = [ "rust-src" "rust-analyzer" ];
        };

        # Linux only. Vulkan is the wrong backend on Darwin (Metal is), and
        # llama.cpp's own nixpkgs throws outright on x86_64-darwin, which
        # nixpkgs 26.11 no longer supports — merely evaluating the attribute
        # would fail the whole flake there.
        hasLlama = pkgs.stdenv.hostPlatform.isLinux
          && llama-cpp.packages ? ${system}
          && llama-cpp.packages.${system} ? vulkan;

        # The WebUI is left on. It costs about nothing once its npm closure
        # is in the store, and turning it off means overriding a package the
        # upstream flake only exposes pre-configured.
        llamaServer = llama-cpp.packages.${system}.vulkan;

        # Flags are measured, not defaults. Each was checked against
        # Qwen3.6-35B-A3B on a Radeon 890M over 24 agent runs:
        #
        #   --jinja               without it the model's tool-call syntax is
        #                         neither produced nor parsed.
        #   --reasoning-preserve  keep prior turns' reasoning; without it a
        #                         multi-turn tool loop collapses after the
        #                         first tool result.
        #   --reasoning-budget    force the think block closed. Uncapped, the
        #                         model spends the whole turn thinking and
        #                         returns neither content nor a tool call:
        #                         2/5 usable turns without it, 5/5 with.
        #   -ngl 999              all layers on the GPU: 205 t/s prefill vs
        #                         98 on CPU. Decode is a wash — the win is
        #                         prompt-heavy work, which is what an agent
        #                         reading files does.
        #   -t 12                 physical cores; 24 (SMT) measured slower.
        #   -c 131072             the model allows 262144; the limit here is
        #                         KV cache against ~31 GiB of GTT, measured
        #                         at ~21.6 KiB/token: 22.6 GiB total at 128k,
        #                         25.4 GiB at 256k. 128k leaves ~9 GiB spare
        #                         and costs nothing in prefill (216 t/s on a
        #                         30k-token request, same as at 32k). The
        #                         earlier 32768 was a benchmark artifact and
        #                         made ordinary sessions fail outright.
        #
        # No Vulkan environment setup: unlike a hand-built binary, this one
        # finds the GPU on its own — VK_ICD_FILENAMES and LD_LIBRARY_PATH are
        # both unnecessary here.
        serve = pkgs.writeShellApplication {
          name = "oneloop-serve";
          runtimeInputs = [ llamaServer pkgs.curl ];
          text = ''
            MODEL="''${ONELOOP_LOCAL_MODEL:-}"
            # An optional leading .gguf path overrides the environment.
            if [ $# -gt 0 ] && [ "''${1##*.}" = "gguf" ]; then
              MODEL="$1"
              shift
            fi

            if [ -z "$MODEL" ]; then
              echo "no model given." >&2
              echo "  oneloop-serve /path/to/model.gguf" >&2
              echo "  ONELOOP_LOCAL_MODEL=/path/to/model.gguf oneloop-serve" >&2
              exit 1
            fi
            [ -f "$MODEL" ] || { echo "no model at $MODEL" >&2; exit 1; }

            PORT="''${ONELOOP_LOCAL_PORT:-8080}"
            # A bind failure surfaces as an opaque socket error several
            # screens into the startup log. Say it plainly instead.
            if curl -s -m 2 "localhost:$PORT/health" 2>/dev/null | grep -q ok; then
              echo "a server is already serving on :$PORT — just use \`ol\`." >&2
              exit 1
            fi

            exec llama-server \
              -m "$MODEL" \
              -ngl 999 -t 12 -c 131072 -n 4096 \
              --jinja --reasoning-preserve --reasoning-budget 600 \
              --host 127.0.0.1 --port "$PORT" --alias local \
              "$@"
          '';
        };
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

        # Deliberately not in devShell: OneLoop talks to an endpoint over
        # HTTP and does not care what serves it, so building an inference
        # engine has no business gating `cargo check`.
        packages = pkgs.lib.optionalAttrs hasLlama {
          inherit serve;
          llama-server = llamaServer;
        };

        apps = pkgs.lib.optionalAttrs hasLlama {
          serve = {
            type = "app";
            program = "${serve}/bin/oneloop-serve";
          };
        };
      }
    );
}
