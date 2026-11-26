# Contributing

## Merge gate

A PR merges when all three pass on the head to merge:

```
nix develop -c cargo fmt --check
nix develop -c cargo test
nix build .#ai-dev
```

## Screenshots

A UI PR updates the image of each screen it changes in `screenshots/` and adds one for a new
screen. Each image is named by the screen it shows and overwritten in place, so the PR's diff shows
before and after. Take them with the dev shell's chromium against the built binary.

## Formatting

`cargo fmt` runs from the dev shell:

```
nix develop -c cargo fmt
```
