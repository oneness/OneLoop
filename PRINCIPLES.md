# OneLoop Principles

This is the soul of the project.

## Keep the core tiny

One loop. Few durable primitives. Avoid frameworks inside the framework.

## Prefer evidence over confidence

If context is needed, gather it. Do not guess.

## The main agent owns safety

Orchestrated providers ask for evidence. They do not get direct tool access.

## Default to readable systems

Simple files, explicit flows, minimal magic.

## Optimize for working in the terminal

Fast feedback, clear output, no hidden state.

## One control knob

Prefer one env var or one directive over a matrix of settings. Simplicity over configurability.

## Cache and share

Gathered evidence is deduplicated and shared. Never fetch the same thing twice.

## Graceful degradation

When something fails — provider, cache, tool — return a clear error. Never panic.
