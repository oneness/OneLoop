{
  description = "oneloop - a local-first coding agent";

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
        #   no -ngl               layer placement is llama.cpp's `--fit` to
        #                         decide, and pinning it is what breaks:
        #                         -ngl 999 turns fitting off outright
        #                         ("n_gpu_layers already set by user to 999,
        #                         abort") and then overcommits whatever
        #                         memory the machine actually has. On a
        #                         24 GiB box — 8 GiB of VRAM carve-out plus
        #                         an 11.5 GiB GTT cap, ~19.5 GiB reachable,
        #                         against 19.0 GiB of weights and a 2.8 GiB
        #                         KV cache — that surfaces as radv "not
        #                         enough memory for command submission" and
        #                         a lost device before the first token.
        #                         Unset, fit puts as much on the GPU as
        #                         fits: 205 t/s prefill where every layer
        #                         lands, 166 t/s on the 24 GiB box (12k
        #                         request) against 109 with --cpu-moe and
        #                         98 on CPU. Decode is a wash — the win is
        #                         prompt-heavy work, which is what an agent
        #                         reading files does.
        #   -t 12                 physical cores; 24 (SMT) measured slower.
        #   -c 131072             the model allows 262144; the limit is KV
        #                         cache against GTT, measured at ~21.6
        #                         KiB/token: 22.6 GiB total at 128k, 25.4
        #                         GiB at 256k. Setting it holds the context
        #                         fixed and leaves fit to trade layers for
        #                         it, which is the right way round — 128k
        #                         costs nothing in prefill (216 t/s on a
        #                         30k-token request, same as at 32k), while
        #                         the earlier 32768 was a benchmark artifact
        #                         and made ordinary sessions fail outright.
        #   -n 32768              output ceiling per response, and the only
        #                         place one is set — the config file names
        #                         no `max_tokens`, so nothing has to be kept
        #                         in sync. The default is -1 (infinity), and
        #                         with context shift disabled that lets a
        #                         model that never emits EOS generate until
        #                         it fills -c: a repetition loop then costs
        #                         the whole window, and the reply is too
        #                         large for the agent to summarize its way
        #                         out of. A quarter of the context bounds
        #                         that while clearing any real answer — the
        #                         earlier 4096 was inherited from a hosted
        #                         API that requires the field, and cut
        #                         large write/edit calls off mid-JSON.
        #
        # No Vulkan environment setup: unlike a hand-built binary, this one
        # finds the GPU on its own — VK_ICD_FILENAMES and LD_LIBRARY_PATH are
        # both unnecessary here.
        serve = pkgs.writeShellApplication {
          name = "oneloop-serve";
          runtimeInputs = [ llamaServer pkgs.curl pkgs.coreutils ];
          text = ''
            # The weights this repo's numbers were measured against. Several
            # other publishers ship a "Qwen3.6-35B-A3B Q4_K_M" that is a
            # different file — 21.2, 22.1 and 22.3 GB variants all exist — so
            # the repo is pinned rather than searched for.
            MODEL_REPO="ggml-org/Qwen3.6-35B-A3B-GGUF"
            MODEL_FILE="Qwen3.6-35B-A3B-Q4_K_M.gguf"
            MODEL_BYTES=20419565568
            DEFAULT_MODEL="$HOME/models/$MODEL_FILE"

            MODEL="''${ONELOOP_LOCAL_MODEL:-$DEFAULT_MODEL}"
            # An optional leading .gguf path overrides the environment.
            if [ $# -gt 0 ] && [ "''${1##*.}" = "gguf" ]; then
              MODEL="$1"
              shift
            fi

            if [ ! -f "$MODEL" ]; then
              # Only the default is on offer; a path the caller chose is
              # theirs to provide, and guessing at a download for it would
              # be presumptuous.
              if [ "$MODEL" != "$DEFAULT_MODEL" ]; then
                echo "no model at $MODEL" >&2
                exit 1
              fi

              SIZE_GB=$(( MODEL_BYTES / 1000000000 ))
              # Never start a multi-gigabyte download nobody is watching:
              # without a terminal there is no one to say no.
              if [ ! -t 0 ]; then
                echo "no model at $MODEL" >&2
                echo "run interactively to be offered the download, or fetch it with:" >&2
                echo "  curl -fL -C - --create-dirs -o $MODEL \\" >&2
                echo "    https://huggingface.co/$MODEL_REPO/resolve/main/$MODEL_FILE" >&2
                exit 1
              fi

              echo "No model at $MODEL"
              echo "  $MODEL_FILE (~''${SIZE_GB} GB) from huggingface.co/$MODEL_REPO"
              printf 'Download it now? [y/N] '
              read -r reply
              case "$reply" in
                [yY]*) ;;
                *) echo "aborted — nothing downloaded." >&2; exit 1 ;;
              esac

              mkdir -p "$(dirname "$MODEL")"
              # Download beside the target, not onto it: an interrupted
              # transfer must never be left looking like a usable model.
              # `-C -` resumes a partial file, which the CDN supports.
              curl -fL -C - --create-dirs --progress-bar \
                -o "$MODEL.part" \
                "https://huggingface.co/$MODEL_REPO/resolve/main/$MODEL_FILE"

              GOT=$(stat -c %s "$MODEL.part")
              if [ "$GOT" != "$MODEL_BYTES" ]; then
                echo "download is $GOT bytes, expected $MODEL_BYTES — leaving $MODEL.part in place" >&2
                echo "re-run to resume it." >&2
                exit 1
              fi
              mv "$MODEL.part" "$MODEL"
              echo "saved $MODEL"
            fi

            PORT="''${ONELOOP_LOCAL_PORT:-8080}"
            # A bind failure surfaces as an opaque socket error several
            # screens into the startup log. Say it plainly instead.
            if curl -s -m 2 "localhost:$PORT/health" 2>/dev/null | grep -q ok; then
              echo "a server is already serving on :$PORT — just use \`ol\`." >&2
              exit 1
            fi

            exec llama-server \
              -m "$MODEL" \
              -t 12 -c 131072 -n 32768 \
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
