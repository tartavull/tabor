TARGET = tabor

ASSETS_DIR = extra
RELEASE_DIR = target/release
MANPAGE = $(ASSETS_DIR)/man/tabor.1.scd
MANPAGE-MSG = $(ASSETS_DIR)/man/tabor-msg.1.scd
MANPAGE-CONFIG = $(ASSETS_DIR)/man/tabor.5.scd
MANPAGE-CONFIG-BINDINGS = $(ASSETS_DIR)/man/tabor-bindings.5.scd
TERMINFO = $(ASSETS_DIR)/tabor.info
COMPLETIONS_DIR = $(ASSETS_DIR)/completions
COMPLETIONS = $(COMPLETIONS_DIR)/_tabor \
	$(COMPLETIONS_DIR)/tabor.bash \
	$(COMPLETIONS_DIR)/tabor.fish

APP_NAME = Tabor.app
APP_TEMPLATE = $(ASSETS_DIR)/osx/$(APP_NAME)
APP_DIR = $(RELEASE_DIR)/osx
APP_BINARY = $(RELEASE_DIR)/$(TARGET)
APP_BINARY_DIR = $(APP_DIR)/$(APP_NAME)/Contents/MacOS
APP_EXTRAS_DIR = $(APP_DIR)/$(APP_NAME)/Contents/Resources
APP_DEFAULT_ENTITLEMENTS = $(ASSETS_DIR)/osx/Tabor.entitlements
APP_PASSKEY_ENTITLEMENTS = $(ASSETS_DIR)/osx/Tabor.passkey.entitlements
APP_RESOURCE_ENTITLEMENTS ?= $(APP_DEFAULT_ENTITLEMENTS)
TABOR_CARGO_FEATURES ?=
TABOR_CODESIGN_ENTITLEMENTS ?=
TABOR_CODESIGN_PROVISIONING_PROFILE ?=
TABOR_CODESIGN_HARDENED_RUNTIME ?= 0
TABOR_CODESIGN_TIMESTAMP ?= 0
TABOR_NOTARY_KEYCHAIN_PROFILE ?=
TABOR_NOTARY_KEYCHAIN ?=
TABOR_NOTARY_TEAM_ID ?=
TABOR_NOTARY_APPLE_ID ?=
TABOR_NOTARY_APP_SPECIFIC_PASSWORD ?=
TABOR_NOTARY_API_KEY_PATH ?=
TABOR_NOTARY_API_KEY_ID ?=
TABOR_NOTARY_API_ISSUER ?=
APP_COMPLETIONS_DIR = $(APP_EXTRAS_DIR)/completions

DMG_NAME = Tabor.dmg
DMG_DIR = $(RELEASE_DIR)/osx

vpath $(TARGET) $(RELEASE_DIR)
vpath $(APP_NAME) $(APP_DIR)
vpath $(DMG_NAME) $(APP_DIR)

all: help

help: ## Print this help message
	@grep -E '^[a-zA-Z._-]+:.*?## .*$$' $(MAKEFILE_LIST) | sort | awk 'BEGIN {FS = ":.*?## "}; {printf "\033[36m%-30s\033[0m %s\n", $$1, $$2}'

binary: $(TARGET)-native ## Build a release binary
binary-universal: $(TARGET)-universal ## Build a universal release binary
app: $(APP_NAME)-native ## Create an Tabor.app
app-universal: $(APP_NAME)-universal ## Create a universal Tabor.app
app-passkey: TABOR_CARGO_FEATURES = --features passkey-webauthn
app-passkey: TABOR_CODESIGN_ENTITLEMENTS = $(APP_PASSKEY_ENTITLEMENTS)
app-passkey: APP_RESOURCE_ENTITLEMENTS = $(APP_PASSKEY_ENTITLEMENTS)
app-passkey: $(APP_NAME)-native ## Create a passkey-enabled Tabor.app (requires TABOR_CODESIGN_PROVISIONING_PROFILE)

app-passkey-universal: TABOR_CARGO_FEATURES = --features passkey-webauthn
app-passkey-universal: TABOR_CODESIGN_ENTITLEMENTS = $(APP_PASSKEY_ENTITLEMENTS)
app-passkey-universal: APP_RESOURCE_ENTITLEMENTS = $(APP_PASSKEY_ENTITLEMENTS)
app-passkey-universal: $(APP_NAME)-universal ## Create a universal passkey-enabled Tabor.app (requires TABOR_CODESIGN_PROVISIONING_PROFILE)

notarize-app: TABOR_CODESIGN_HARDENED_RUNTIME = 1
notarize-app: TABOR_CODESIGN_TIMESTAMP = 1
notarize-app: $(APP_NAME)-native ## Build and notarize Tabor.app
	@TABOR_NOTARY_KEYCHAIN_PROFILE="$(TABOR_NOTARY_KEYCHAIN_PROFILE)" TABOR_NOTARY_KEYCHAIN="$(TABOR_NOTARY_KEYCHAIN)" TABOR_NOTARY_TEAM_ID="$(TABOR_NOTARY_TEAM_ID)" TABOR_NOTARY_APPLE_ID="$(TABOR_NOTARY_APPLE_ID)" TABOR_NOTARY_APP_SPECIFIC_PASSWORD="$(TABOR_NOTARY_APP_SPECIFIC_PASSWORD)" TABOR_NOTARY_API_KEY_PATH="$(TABOR_NOTARY_API_KEY_PATH)" TABOR_NOTARY_API_KEY_ID="$(TABOR_NOTARY_API_KEY_ID)" TABOR_NOTARY_API_ISSUER="$(TABOR_NOTARY_API_ISSUER)" scripts/notarize-macos-app.sh $(APP_DIR)/$(APP_NAME)

notarize-app-universal: TABOR_CODESIGN_HARDENED_RUNTIME = 1
notarize-app-universal: TABOR_CODESIGN_TIMESTAMP = 1
notarize-app-universal: $(APP_NAME)-universal ## Build and notarize universal Tabor.app
	@TABOR_NOTARY_KEYCHAIN_PROFILE="$(TABOR_NOTARY_KEYCHAIN_PROFILE)" TABOR_NOTARY_KEYCHAIN="$(TABOR_NOTARY_KEYCHAIN)" TABOR_NOTARY_TEAM_ID="$(TABOR_NOTARY_TEAM_ID)" TABOR_NOTARY_APPLE_ID="$(TABOR_NOTARY_APPLE_ID)" TABOR_NOTARY_APP_SPECIFIC_PASSWORD="$(TABOR_NOTARY_APP_SPECIFIC_PASSWORD)" TABOR_NOTARY_API_KEY_PATH="$(TABOR_NOTARY_API_KEY_PATH)" TABOR_NOTARY_API_KEY_ID="$(TABOR_NOTARY_API_KEY_ID)" TABOR_NOTARY_API_ISSUER="$(TABOR_NOTARY_API_ISSUER)" scripts/notarize-macos-app.sh $(APP_DIR)/$(APP_NAME)

notarize-dmg: TABOR_CODESIGN_HARDENED_RUNTIME = 1
notarize-dmg: TABOR_CODESIGN_TIMESTAMP = 1
notarize-dmg: $(DMG_NAME)-native ## Build and notarize Tabor.app, then staple Tabor.dmg
	@TABOR_NOTARY_KEYCHAIN_PROFILE="$(TABOR_NOTARY_KEYCHAIN_PROFILE)" TABOR_NOTARY_KEYCHAIN="$(TABOR_NOTARY_KEYCHAIN)" TABOR_NOTARY_TEAM_ID="$(TABOR_NOTARY_TEAM_ID)" TABOR_NOTARY_APPLE_ID="$(TABOR_NOTARY_APPLE_ID)" TABOR_NOTARY_APP_SPECIFIC_PASSWORD="$(TABOR_NOTARY_APP_SPECIFIC_PASSWORD)" TABOR_NOTARY_API_KEY_PATH="$(TABOR_NOTARY_API_KEY_PATH)" TABOR_NOTARY_API_KEY_ID="$(TABOR_NOTARY_API_KEY_ID)" TABOR_NOTARY_API_ISSUER="$(TABOR_NOTARY_API_ISSUER)" scripts/notarize-macos-app.sh $(APP_DIR)/$(APP_NAME) $(DMG_DIR)/$(DMG_NAME)

notarize-dmg-universal: TABOR_CODESIGN_HARDENED_RUNTIME = 1
notarize-dmg-universal: TABOR_CODESIGN_TIMESTAMP = 1
notarize-dmg-universal: $(DMG_NAME)-universal ## Build and notarize universal Tabor.app, then staple Tabor.dmg
	@TABOR_NOTARY_KEYCHAIN_PROFILE="$(TABOR_NOTARY_KEYCHAIN_PROFILE)" TABOR_NOTARY_KEYCHAIN="$(TABOR_NOTARY_KEYCHAIN)" TABOR_NOTARY_TEAM_ID="$(TABOR_NOTARY_TEAM_ID)" TABOR_NOTARY_APPLE_ID="$(TABOR_NOTARY_APPLE_ID)" TABOR_NOTARY_APP_SPECIFIC_PASSWORD="$(TABOR_NOTARY_APP_SPECIFIC_PASSWORD)" TABOR_NOTARY_API_KEY_PATH="$(TABOR_NOTARY_API_KEY_PATH)" TABOR_NOTARY_API_KEY_ID="$(TABOR_NOTARY_API_KEY_ID)" TABOR_NOTARY_API_ISSUER="$(TABOR_NOTARY_API_ISSUER)" scripts/notarize-macos-app.sh $(APP_DIR)/$(APP_NAME) $(DMG_DIR)/$(DMG_NAME)
$(TARGET)-native:
	MACOSX_DEPLOYMENT_TARGET="10.12" cargo build --release $(TABOR_CARGO_FEATURES)
$(TARGET)-universal:
	MACOSX_DEPLOYMENT_TARGET="10.12" cargo build --release --target=x86_64-apple-darwin $(TABOR_CARGO_FEATURES)
	MACOSX_DEPLOYMENT_TARGET="10.12" cargo build --release --target=aarch64-apple-darwin $(TABOR_CARGO_FEATURES)
	@lipo target/{x86_64,aarch64}-apple-darwin/release/$(TARGET) -create -output $(APP_BINARY)

$(APP_NAME)-%: $(TARGET)-%
	@mkdir -p $(APP_BINARY_DIR)
	@mkdir -p $(APP_EXTRAS_DIR)
	@mkdir -p $(APP_COMPLETIONS_DIR)
	@scdoc < $(MANPAGE) | gzip -c > $(APP_EXTRAS_DIR)/tabor.1.gz
	@scdoc < $(MANPAGE-MSG) | gzip -c > $(APP_EXTRAS_DIR)/tabor-msg.1.gz
	@scdoc < $(MANPAGE-CONFIG) | gzip -c > $(APP_EXTRAS_DIR)/tabor.5.gz
	@scdoc < $(MANPAGE-CONFIG-BINDINGS) | gzip -c > $(APP_EXTRAS_DIR)/tabor-bindings.5.gz
	@tic -xe tabor,tabor-direct -o $(APP_EXTRAS_DIR) $(TERMINFO)
	@cp -fRp $(APP_TEMPLATE) $(APP_DIR)
	@cp -fp $(APP_BINARY) $(APP_BINARY_DIR)
	@cp -fp $(APP_RESOURCE_ENTITLEMENTS) $(APP_EXTRAS_DIR)/Tabor.entitlements
	@scripts/bundle-macos-deps.sh $(APP_DIR)/$(APP_NAME)
	@scripts/create-macos-cef-helpers.sh $(APP_DIR)/$(APP_NAME)
	@cp -fp $(COMPLETIONS) $(APP_COMPLETIONS_DIR)
	@TABOR_CODESIGN_ENTITLEMENTS="$(TABOR_CODESIGN_ENTITLEMENTS)" TABOR_CODESIGN_PROVISIONING_PROFILE="$(TABOR_CODESIGN_PROVISIONING_PROFILE)" TABOR_CODESIGN_HARDENED_RUNTIME="$(TABOR_CODESIGN_HARDENED_RUNTIME)" TABOR_CODESIGN_TIMESTAMP="$(TABOR_CODESIGN_TIMESTAMP)" scripts/sign-macos-app.sh $(APP_DIR)/$(APP_NAME)
	@touch -r "$(APP_BINARY)" "$(APP_DIR)/$(APP_NAME)"
	@echo "Created '$(APP_NAME)' in '$(APP_DIR)'"

dmg: $(DMG_NAME)-native ## Create an Tabor.dmg
dmg-universal: $(DMG_NAME)-universal ## Create a universal Tabor.dmg
$(DMG_NAME)-%: $(APP_NAME)-%
	@echo "Packing disk image..."
	@ln -sf /Applications $(DMG_DIR)/Applications
	@hdiutil create $(DMG_DIR)/$(DMG_NAME) \
		-volname "Tabor" \
		-fs HFS+ \
		-srcfolder $(APP_DIR) \
		-ov -format UDZO
	@echo "Packed '$(APP_NAME)' in '$(APP_DIR)'"

install: $(INSTALL)-native ## Mount disk image
install-universal: $(INSTALL)-native ## Mount universal disk image
$(INSTALL)-%: $(DMG_NAME)-%
	@open $(DMG_DIR)/$(DMG_NAME)

.PHONY: app app-passkey app-passkey-universal app-universal binary clean dmg install notarize-app notarize-app-universal notarize-dmg notarize-dmg-universal $(TARGET) $(TARGET)-universal

clean: ## Remove all build artifacts
	@cargo clean
