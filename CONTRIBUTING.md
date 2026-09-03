# Contributing

The project-wide contributor guide — how to run the simulator with no hardware,
why the licence boundary sits where it does, the four design rules every change
is checked against, and how cross-repo protocol changes flow — lives in the
meta-repo:

  https://github.com/sverredraaisma/Lumen-Dev/blob/main/CONTRIBUTING.md

Specific to **lumen-device**: see `README.md` in this directory.

Working across repo boundaries (this repo plus its dependencies in one checkout,
one `cargo test`) is what `lumen-dev` is for. Clone it and run
`scripts/clone-all.sh`.
