# Quick MQTT CLI
![Rust](https://github.com/EdJoPaTo/quick-mqtt-cli/workflows/Rust/badge.svg)

> Small Command Line Utility to quickly publish or subscribe a given mqtt topic

## Usage

```sh
# Subscribe to topic
quick-mqtt "topic"

# Publish to topic
quick-mqtt "topic" "payload"

# Subscribe to topic with a specific host (default is localhost)
quick-mqtt -h "test.mosquitto.org" "hello/world"
```

```plaintext
Quick MQTT CLI 0.1.0
EdJoPaTo <quick-mqtt-cli-rust@edjopato.de>
Small Command Line Utility to quickly publish or subscribe something to a given mqtt topic

USAGE:
    quick-mqtt [FLAGS] [OPTIONS] <TOPIC> [PAYLOAD]

FLAGS:
        --help       Prints help information
    -V, --version    Prints version information
    -v, --verbose    Show full MQTT communication

OPTIONS:
    -h, --host <HOST>    Host on which the MQTT Broker is running [default: localhost]
    -p, --port <INT>     Port on which the MQTT Broker is running [default: 1883]

ARGS:
    <TOPIC>      Topic to watch or publish to
    <PAYLOAD>    (optional) Payload to be published. If none is given it is instead subscribed to the topic.
```
