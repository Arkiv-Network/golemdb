import "./just/build.just"
import "./just/code_check.just"
import "./just/test.just"
import "./just/bench.just"

export RUST_BACKTRACE := "full"

# Lists all the available commands
default:
  @just --list
