# Emacs

Inspect the running Emacs through the `elisp` tool: buffers, windows, cursor positions, diagnostics, process output, and unsaved edits.

Load this skill before using `elisp`. The expression runs in the Emacs server, not in the user's selected buffer.

## Rules

- Always name buffers with `with-current-buffer`; the server's current buffer is internal.
- Check that `(get-buffer NAME)` is non-nil before using it.
- Use `window-point` to inspect where the user is looking.
- Return one string per call. Use `mapconcat`, `format`, and `buffer-substring-no-properties`.
- Keep every operation bounded. Take snapshots of process buffers; never install a polling loop.
- Never prompt: do not use `read-string`, `y-or-n-p`, `completing-read`, or potentially prompting `call-interactively`.
- Never use an unbounded loop. A client timeout cannot stop Lisp already executing in Emacs.
- Never call `kill-emacs`, `save-buffers-kill-terminal`, or kill the user's buffers.
- Do not modify buffers, save files, or rearrange windows unless the user explicitly asks.
- Use `read`, `write`, and `edit` for files on disk. Use `elisp` for editor-only state.
- Do not run shell commands through Emacs; use `bash`.

## Discover buffers

List non-internal buffers:

```elisp
(mapconcat #'buffer-name
           (seq-remove (lambda (b) (string-prefix-p " " (buffer-name b)))
                       (buffer-list))
           "\n")
```

Find an approximately named buffer:

```elisp
(mapconcat #'buffer-name
           (seq-filter (lambda (b) (string-match-p "serve" (buffer-name b)))
                       (buffer-list))
           "\n")
```

## Read editor state

Read a buffer without text properties:

```elisp
(let ((b (get-buffer "main.rs")))
  (if b
      (with-current-buffer b
        (buffer-substring-no-properties (point-min) (point-max)))
    "buffer not found"))
```

Tail a process buffer:

```elisp
(with-current-buffer "*compilation*"
  (buffer-substring-no-properties
   (max (point-min) (- (point-max) 8000))
   (point-max)))
```

Describe the selected window:

```elisp
(let* ((w (selected-window)) (b (window-buffer w)))
  (with-current-buffer b
    (format "%s %s line %d"
            (buffer-name b)
            (or buffer-file-name "no file")
            (line-number-at-pos (window-point w)))))
```

List visible buffers:

```elisp
(mapconcat (lambda (w)
             (format "%s%s"
                     (buffer-name (window-buffer w))
                     (if (eq w (selected-window)) " [selected]" "")))
           (window-list)
           "\n")
```

## Unsaved files

A modified file-visiting buffer differs from the file on disk. Check before reading a relevant file:

```elisp
(mapconcat (lambda (b)
             (format "%s  %s" (buffer-name b) (buffer-file-name b)))
           (seq-filter (lambda (b)
                         (and (buffer-file-name b) (buffer-modified-p b)))
                       (buffer-list))
           "\n")
```

If the relevant buffer is modified, read the buffer and state that it—not the disk file—was used.

## Processes and diagnostics

List live and completed Emacs processes:

```elisp
(mapconcat (lambda (p)
             (format "%s  %s  %s"
                     (process-name p)
                     (process-status p)
                     (if (process-buffer p)
                         (buffer-name (process-buffer p))
                       "no buffer")))
           (process-list)
           "\n")
```

Read Flymake diagnostics when Flymake is active:

```elisp
(with-current-buffer "main.rs"
  (if flymake-mode
      (mapconcat (lambda (d)
                   (format "%d: %s"
                           (line-number-at-pos (flymake-diagnostic-beg d))
                           (flymake-diagnostic-text d)))
                 (flymake-diagnostics)
                 "\n")
    "flymake is not active"))
```

## Paths

Expand paths before passing them from Emacs to another tool:

```elisp
(with-current-buffer "main.rs"
  (expand-file-name (project-root (project-current))))
```

A file-visiting buffer can point to a file that no longer exists. Verify with `file-exists-p` before reconciling editor and disk content.
