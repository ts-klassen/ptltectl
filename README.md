# ptltectl

Command-line controller for the Patlite LR6-USB tower that mirrors the Erlang API provided by `ptlte`.

## Build

```bash
cargo build --release
```

## Usage

All commands require access to the tower (see the udev rule in the main `ptlte` README).

```bash
# Turn the red LED solid on
cargo run -- light red led_on

# Set all LEDs at once (red yellow green blue white)
cargo run -- tower led_on led_off led_off led_off led_off

# Buzzer pattern 2, limit 3, default pitches
cargo run -- buzzer buzz_pattern2 3

# Explicit pitches
cargo run -- buzzer buzz_on 0x2 0x6 0x0

# Raw 8-byte HID report
cargo run -- report 0 0 15 0 31 255 240 0

# Reset everything
cargo run -- reset
```
