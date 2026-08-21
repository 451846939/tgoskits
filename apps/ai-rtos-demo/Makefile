CC ?= cc
CROSS_CC ?= aarch64-linux-musl-gcc
CFLAGS ?= -O2 -g -Wall -Wextra -Werror -std=c11
CPPFLAGS += -I./aicp -I./ai-model -I./rtos-core
LDLIBS += -lm
AICP_CLIENT_CORE := aicp/aicp_client.c
AICP_PROTOCOL_HEADERS := aicp/aicp.h aicp/aicp_stream.h aicp/aicp_datagram.h
AICP_POSIX_HEADERS := $(AICP_PROTOCOL_HEADERS) aicp/aicp_posix_stream.h
AICP_SERVICE_CORE := rtos-core/aicp_service.c rtos-core/control_loop.c
AICP_SERVICE_HEADERS := rtos-core/aicp_service.h rtos-core/control_loop.h
AICP_CONTROL_POLICY := ai-model/control_policy.c ai-model/simple_nn.c
AICP_CONTROL_POLICY_HEADERS := ai-model/control_policy.h ai-model/simple_nn.h
AICP_STARRY_CPPFLAGS ?= -include linux-init/starry_profile.h
AICP_STARRY_DEFS ?=

BIN_DIR ?= build
AARCH64_BIN_DIR ?= $(BIN_DIR)/aarch64
CLIENT := $(BIN_DIR)/aicp_client
SERVER := $(BIN_DIR)/aicp_server
MODEL_RUNNER := $(BIN_DIR)/aicp_model_runner
PROTO_TEST := $(BIN_DIR)/aicp_protocol_test
SERVICE_TEST := $(BIN_DIR)/aicp_service_test
CLIENT_TEST := $(BIN_DIR)/aicp_client_test
RUST_CLIENT := $(BIN_DIR)/aicp_rust_client
RUST_AARCH64_CLIENT := $(AARCH64_BIN_DIR)/aicp_rust_client
AARCH64_CLIENT := $(AARCH64_BIN_DIR)/aicp_client
AARCH64_SERVER := $(AARCH64_BIN_DIR)/aicp_server
AARCH64_INIT := $(AARCH64_BIN_DIR)/aicp_init
AARCH64_STARRY_INIT := $(AARCH64_BIN_DIR)/aicp_starry_init
AARCH64_YOLO_INIT := $(AARCH64_BIN_DIR)/aicp_yolov8_init
AARCH64_MODEL_RUNNER := $(AARCH64_BIN_DIR)/aicp_model_runner

.PHONY: all clean smoke protocol-test service-test client-test test model-runner model-compare rust-client rust-client-aarch64 model-runner-aarch64 cross-aarch64 linux-init-aarch64 starry-init-aarch64 yolov8-init-aarch64

all: $(CLIENT) $(SERVER) $(MODEL_RUNNER)

$(BIN_DIR):
	mkdir -p $@

$(AARCH64_BIN_DIR):
	mkdir -p $@

$(CLIENT): linux-client/main.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) $(AICP_CLIENT_CORE) aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ linux-client/main.c $(AICP_CLIENT_CORE) $(AICP_CONTROL_POLICY) $(LDLIBS)

$(SERVER): host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(AICP_SERVICE_HEADERS) $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(LDLIBS)

$(MODEL_RUNNER): model-runner/aicp_model_runner.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) aicp/aicp.h | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ model-runner/aicp_model_runner.c $(AICP_CONTROL_POLICY) $(LDLIBS)

$(PROTO_TEST): tests/aicp_protocol_test.c $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ $<

$(SERVICE_TEST): tests/aicp_service_test.c $(AICP_SERVICE_CORE) $(AICP_SERVICE_HEADERS) $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -pthread -o $@ tests/aicp_service_test.c $(AICP_SERVICE_CORE) $(LDLIBS)

$(CLIENT_TEST): tests/aicp_client_test.c $(AICP_CLIENT_CORE) aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -pthread -o $@ tests/aicp_client_test.c $(AICP_CLIENT_CORE)

$(RUST_CLIENT): rust-client/src/main.rs rust-client/Cargo.toml ../../components/aicp-protocol/src/lib.rs ../../components/aicp-protocol/Cargo.toml | $(BIN_DIR)
	cargo build --manifest-path rust-client/Cargo.toml --release
	cp rust-client/target/release/aicp_rust_client $@

rust-client: $(RUST_CLIENT)

$(RUST_AARCH64_CLIENT): rust-client/src/main.rs rust-client/Cargo.toml rust-client/build-aarch64.sh rust-client/build-aarch64-docker.sh ../../components/aicp-protocol/src/lib.rs ../../components/aicp-protocol/Cargo.toml | $(AARCH64_BIN_DIR)
	AICP_RUST_AARCH64_OUT="$(abspath $@)" rust-client/build-aarch64.sh

rust-client-aarch64: $(RUST_AARCH64_CLIENT)

protocol-test: $(PROTO_TEST)
	$(PROTO_TEST)

service-test: $(SERVICE_TEST)
	$(SERVICE_TEST)

client-test: $(CLIENT_TEST)
	$(CLIENT_TEST)

test: protocol-test service-test client-test

model-runner: $(MODEL_RUNNER)
	$(MODEL_RUNNER)

model-compare: $(MODEL_RUNNER)
	$(MODEL_RUNNER) --steps 64 --mode ai --csv $(BIN_DIR)/model-ai.csv
	$(MODEL_RUNNER) --steps 64 --mode fixed --csv $(BIN_DIR)/model-fixed.csv

$(AARCH64_CLIENT): linux-client/main.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) $(AICP_CLIENT_CORE) aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -o $@ linux-client/main.c $(AICP_CLIENT_CORE) $(AICP_CONTROL_POLICY) $(LDLIBS)

$(AARCH64_SERVER): host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(AICP_SERVICE_HEADERS) $(AICP_POSIX_HEADERS) | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -o $@ host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(LDLIBS)

$(AARCH64_INIT): linux-init/aicp_init.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) $(AICP_CLIENT_CORE) aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -static -o $@ linux-init/aicp_init.c $(AICP_CLIENT_CORE) $(AICP_CONTROL_POLICY) $(LDLIBS)

$(AARCH64_STARRY_INIT): linux-init/aicp_init.c linux-init/starry_profile.h $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) $(AICP_CLIENT_CORE) aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(AICP_STARRY_CPPFLAGS) $(AICP_STARRY_DEFS) $(CFLAGS) -static -o $@ linux-init/aicp_init.c $(AICP_CLIENT_CORE) $(AICP_CONTROL_POLICY) $(LDLIBS)

$(AARCH64_YOLO_INIT): yolov8-init/aicp_yolov8_init.c | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -static -o $@ $<

$(AARCH64_MODEL_RUNNER): model-runner/aicp_model_runner.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) aicp/aicp.h | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -static -o $@ model-runner/aicp_model_runner.c $(AICP_CONTROL_POLICY) $(LDLIBS)

linux-init-aarch64: $(AARCH64_INIT)

starry-init-aarch64: $(AARCH64_STARRY_INIT)

yolov8-init-aarch64: $(AARCH64_YOLO_INIT)

model-runner-aarch64: $(AARCH64_MODEL_RUNNER)

cross-aarch64: $(AARCH64_CLIENT) $(AARCH64_SERVER) $(AARCH64_INIT) $(AARCH64_STARRY_INIT) $(AARCH64_YOLO_INIT) $(AARCH64_MODEL_RUNNER)

smoke: all
	../../scripts/ai-rtos/aicp.sh smoke

clean:
	rm -rf $(BIN_DIR)
