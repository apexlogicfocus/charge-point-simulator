# ⚡ Flowion Charge Point Simulator

> **A high-fidelity OCPP Charge Point Simulator for testing, validating, and developing EV charging infrastructure.**

[![Rust](https://img.shields.io/badge/rust-stable-orange.svg)](https://www.rust-lang.org/)
[![License](https://img.shields.io/badge/license-MIT%20%2F%20Apache--2.0-blue.svg)](#license)
[![Built With Ratatui](https://img.shields.io/badge/Built_With_Ratatui-000?logo=ratatui&logoColor=fff)](https://ratatui.rs/)

---

## 🚀 Overview

**Flowion Charge Point Simulator** is an open-source simulator that emulates real-world EV charge points using the Open Charge Point Protocol (OCPP).

It enables developers to test and validate **Charge Station Management Systems (CSMS)**, develop smart charging algorithms, verify load balancing strategies, and automate integration testing — without requiring physical charging hardware.

Built with protocol correctness, realism, and developer experience in mind, Flowion Charge Point Simulator aims to behave like an actual charging station rather than simply replaying OCPP messages.

The simulator currently supports **OCPP 1.6J**, with **OCPP 2.0.1 and OCPP 2.1 actively being implemented**.

Whether you are developing a new CSMS, validating an existing backend, or testing complex charging scenarios, Flowion Charge Point Simulator provides a fast, deterministic, and scriptable environment for EV charging development.

---

## 🤔 Why Flowion Charge Point Simulator?

Developing and testing against physical charge points can be expensive, slow, and difficult to automate.

Flowion Charge Point Simulator provides a reliable virtual alternative that integrates seamlessly into your development workflow.

Designed by **Flowion AB**, the simulator focuses on **protocol accuracy** and realistic charge point behavior. Instead of simply sending predefined messages, it aims to replicate how a real charging station communicates, reacts, and operates.

| Physical Charge Point                    | Flowion Charge Point Simulator             |
| ---------------------------------------- | ------------------------------------------ |
| 💰 Requires purchasing hardware          | ✅ No hardware required                     |
| 📍 Must be physically accessible         | ✅ Runs anywhere                            |
| 🔄 Difficult to reproduce edge cases     | ✅ Deterministic and repeatable scenarios   |
| 🧪 Limited automation capabilities       | ✅ Designed for automated testing and CI    |
| ⚙️ Firmware-dependent behavior           | ✅ Fully configurable                       |
| ⏱️ Slow to reset and reproduce scenarios | ✅ Starts in seconds                        |
| 📦 One charger per physical device       | ✅ Simulate multiple virtual charge points  |
| 🐞 Difficult to trigger failures         | ✅ Simulate protocol and charging scenarios |
| 🌐 Requires dedicated infrastructure     | ✅ Run locally or against remote systems    |

---

## ✨ Features

* ⚡ **High-fidelity charge point simulation**
* 🔌 OCPP **1.6J** support
* 🚀 OCPP **2.0.1 implementation in progress**
* 🔮 OCPP **2.1 implementation in progress**
* 🌐 WebSocket and Secure WebSocket (WSS)
* 🖥️ Interactive Ratatui terminal dashboard
* 📊 Real-time charger monitoring
* 📜 Live protocol logging
* 🎛️ Interactive charger control
* 🔋 Realistic charging lifecycle simulation
* 🧩 Scriptable scenarios
* 🪶 Lightweight and fast
* 🤖 CI-friendly automation

---

## 🎯 Use Cases

Flowion Charge Point Simulator is designed for:

* 🚀 Developing OCPP backends
* 🧪 CSMS integration testing
* ⚡ Testing smart charging algorithms
* 🔋 Validating load balancing solutions
* 📈 Performance and scalability testing
* 🤖 Automated regression testing
* 🔄 Continuous Integration pipelines
* 🎓 Learning and experimenting with OCPP
* 🏢 Demonstrating EV charging solutions without hardware

---

## 🔌 Supported Protocols

| Protocol   | Status         |
| ---------- | -------------- |
| OCPP 1.6J  | ✅ Supported    |
| OCPP 2.0.1 | 🚧 In Progress |
| OCPP 2.1   | 🚧 In Progress |

### Transport

| Transport              | Status      |
| ---------------------- | ----------- |
| WebSocket              | ✅ Supported |
| Secure WebSocket (WSS) | ✅ Supported |

---

## 🔋 Simulation Capabilities

Flowion Charge Point Simulator is designed to simulate the complete behavior of a real charging station.

Supported scenarios include:

* 🔔 Boot Notification
* ❤️ Heartbeat
* 🔐 Authorization
* 🔌 Start and stop transactions
* 📊 Meter values
* 🚦 Status notifications
* ⚡ Smart charging
* ▶️ Remote start transaction
* ⏹️ Remote stop transaction
* 🔄 Reset operations
* 📦 Firmware updates
* 🩺 Diagnostics
* 📢 Trigger messages
* 📅 Reservations
* 🔌 Plug and unplug events
* ⚠️ Fault scenarios

---

## 📦 Installation

### Homebrew

```bash
brew install flowion-charge-point-simulator
```

### Build from Source

Requirements:

* Rust toolchain
* Cargo

```bash
git clone https://github.com/flowion/flowion-charge-point-simulator.git

cd flowion-charge-point-simulator

cargo build --release
```

Run:

```bash
cargo run --release
```

---

## ▶️ Quick Start

Start the simulator:

```bash
flowion-charge-point-simulator
```

The simulator launches an interactive terminal dashboard where you can monitor and control simulated charge points.

---

## ⚙️ Configuration

Configuration is provided using YAML files.

Example:

```yaml
server:
  url: wss://localhost:9000/ocpp

charge_points:
  - id: CP001
    protocol: ocpp1.6j
```

More examples and configuration documentation will be added as the project evolves.

---

## 💾 State and persistence

Every simulated charger - whether it's dialed against a CSMS or running fully offline as a local
simulation - persists its hardware-backed state (boot reason, in-flight transactions, the
authorization cache, the device model, the security event log, ...) to disk by default. This
includes an unconnected/local charger: it's the one people tend to leave running, so surviving a
restart is exactly where that persistence earns its keep, and it now gets the same treatment a
CSMS-connected charger does.

State lives under one directory per charger:

```
<config dir>/flowion-charge-point-simulator/storage/<charger-id>/
```

`<config dir>` follows your OS's usual convention (e.g. `~/.config` on Linux, `~/Library/Application
Support` on macOS) unless the `FLOWION_STATE_DIR` environment variable is set, in which case state is
written under `<FLOWION_STATE_DIR>/storage/<charger-id>/` instead - handy for tests, CI, or running
multiple isolated instances side by side.

Whether anything actually gets written still depends on the charger's own declared
`capabilities.has_persistent_storage`/`has_display` - the directory is only ever read from or written
to when the charger's configuration says to.

---

## 🖥️ Dashboard

The built-in Ratatui dashboard provides:

* 📊 Charge point overview
* 🔌 Connector states, including the cable lock, the contactor, and any current limit a smart
  charging profile has applied
* ⚡ Charging sessions
* 📈 Meter values, and the separate export register when a connector is discharging
* 📦 Firmware installs and file transfers in flight
* 📋 What the selected charger declares to the CSMS
* 📜 Live logs
* ⌨️ Keyboard controls (`?` lists them all)
* 🔍 Real-time protocol events

### Driving the hardware directly

The command palette (`Ctrl+K`, or `c`) lists two kinds of thing: OCPP commands, which go to the
charger's own state machine, and hardware actions, which go straight to its simulated hardware
because OCPP has no way to ask for them. On a charger that declares the matching capability, those
are:

* **Toggle V2G discharge** (`d` is a shortcut) - see below
* **Install firmware locally** - fetches an image and installs it, with no CSMS campaign behind
  either half, so a firmware install can be watched offline
* **Upload diagnostics locally** - the same for a log upload
* **Fail firmware installs / downloads / uploads** - arms a deliberate, reproducible failure, so a
  CSMS's `InstallationFailed`/`DownloadFailed`/`UploadFailure` handling can be exercised on demand.
  Each is armed until the charger is restarted, and each half fails independently

An action a charger's configuration doesn't declare is simply not listed.

### Bidirectional power (V2G)

`d` puts the focused connector into export and back, on a charger whose configuration declares
`capabilities.supports_bidirectional_power` and with a vehicle plugged in. This is a direct hardware
action, not an OCPP one - no OCPP message can carry a power direction, so a CSMS cannot ask a charger
to export today. The metering itself is real: exported energy accumulates in its own register while
OCPP's import register correctly freezes rather than running backwards.

### Built-in presets

Three chargers ship without any YAML: a plain OCPP 1.6J single-connector charger, a plain 2.0.1
dual-EVSE one, and `demo-ocpp21-full`, which declares every capability the simulator has simulated
hardware behind - smart charging, V2G, DER control, reservations, the local authorization list,
firmware management, diagnostics, certificate management and persistent storage - so all of it can be
exercised without writing a config first.

---

## 🧪 Testing & CI

Flowion Charge Point Simulator is designed to integrate into automated development workflows.

Common use cases:

* End-to-end CSMS testing
* Protocol validation
* Regression testing
* Automated charging scenarios
* Performance testing
* CI/CD pipelines

---

## 🛣️ Roadmap

Planned improvements:

* 🧩 Plugin system
* 📚 More scenario examples
* 🎬 Demo scenarios
* 📦 Improved binary distribution
* 🧪 Expanded automated test scenarios
* ⚡ Continued OCPP 2.0.1 and OCPP 2.1 development

---

## 🤝 Contributing

Contributions are welcome!

You can help by:

* 🐛 Reporting bugs
* 💡 Suggesting features
* 📝 Improving documentation
* 🔧 Submitting pull requests

Contribution guidelines will be added soon.

---

## 📄 License

Flowion Charge Point Simulator is dual licensed:

* MIT License
* Apache License 2.0

You may choose either license.

---

## 🏢 About Flowion

**Flowion Charge Point Simulator** is developed by **Flowion AB** as part of our mission to make EV charging infrastructure more accessible, reliable, and developer-friendly.

Flowion builds modern software solutions for electric vehicle charging, focusing on open standards such as **OCPP** and helping companies develop and operate scalable charging solutions.

The Charge Point Simulator represents our commitment to providing high-quality developer tools that make it easier to build, test, and validate EV charging systems.

---

## ⭐ Support the Project

If you find this project useful:

* ⭐ Star the repository
* 🐛 Report issues
* 💡 Suggest improvements
* 🤝 Contribute

Together we can make EV charging development easier and more accessible.
