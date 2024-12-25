# teensy-keyboard-bridge
hmm teensy press keys time

## Building

First install the required bits:
```sh
rustup target add thumbv7em-none-eabihf
rustup component add llvm-tools-preview
cargo install cargo-binutils
```

You will need the teensy_loader_cli program as well, there is an AUR repo of the same name which I installed.

Now to build and flash the teensy:
```sh
cd firmware

# Build the firmware hex file
cargo objcopy --release -- -O ihex firmware.hex 

# Flash the firmware
teensy_loader_cli --mcu=TEENSY41 -w firmware.hex
```

Connect the serial over USB to the teensy and whatever computer you want to send keypresses from.
Now connect the teensy to the computer you want to send keypresses to.

An example setup could look like this:
[Image shows a teensy 4.1 connected to one computer via a USB cable and to another via a USB to serial cable](example.jpg)

You can now run the client:
```sh
cd client
cargo run --release
```

It will pop up a dialog if there is ambiguity about what serial port to send over or what keyboard to read keypresses from.
