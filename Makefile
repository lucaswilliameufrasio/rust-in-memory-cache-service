setup:
	$ cargo install -f cargo-upgrades
	$ cargo install cargo-edit
	$ cargo install cargo-watch
.PHONY: setup

start:
	cargo run --release
.PHONY: start

build:
	cargo build --release
.PHONY: build

dev:
	RUST_LOG=debug cargo watch -x 'run'
.PHONY: dev

build-image:
	docker build -t lucaswilliameufrasio/cache-service-development .
.PHONY: build-image

update-dependencies-on-lockfile:
	cargo update
.PHONY: update-dependencies-on-lockfile

upgrade-dependencies:
	cargo upgrade && cargo upgrade
.PHONY: upgrade-dependencies
