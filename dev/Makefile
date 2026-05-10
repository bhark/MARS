.PHONY: dev-up dev-down dev-logs dev-reseed dev-purge dev-build dev-install

dev-build:
	./build.sh

dev-install:
	./install.sh

dev-up: dev-build dev-install
	systemctl --user start mars-viewer.service

dev-down:
	systemctl --user stop mars-viewer.service mars-runtime.service mars-compiler.service mars-seed.service mars-postgis.service || true

dev-logs:
	journalctl --user -u 'mars-*' -f

dev-reseed:
	systemctl --user stop mars-seed.service || true
	systemctl --user start mars-seed.service

dev-purge: dev-down
	./uninstall.sh
	podman volume rm -f mars-artifact-store mars-postgis-data mars-osm-cache mars-compiler-cache mars-compiler-work mars-runtime-cache 2>/dev/null || true
