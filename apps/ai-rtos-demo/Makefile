CC ?= cc
CROSS_CC ?= aarch64-linux-musl-gcc
CFLAGS ?= -O2 -g -Wall -Wextra -Werror -std=c11
CPPFLAGS += -I./aicp -I./ai-model -I./rtos-core
LDLIBS += -lm

BIN_DIR ?= build
PROTO_TEST := $(BIN_DIR)/aicp_protocol_test
SERVICE_TEST := $(BIN_DIR)/aicp_service_test
CLIENT_TEST := $(BIN_DIR)/aicp_client_test
MODEL_TEST := $(BIN_DIR)/aicp_model_test
REFERENCE_SERVER := $(BIN_DIR)/aicp_reference_server
AARCH64_BIN_DIR ?= $(BIN_DIR)/aarch64
AARCH64_INIT := $(AARCH64_BIN_DIR)/aicp_init

AICP_PROTOCOL_HEADERS := aicp/aicp.h aicp/aicp_stream.h aicp/aicp_datagram.h
AICP_POSIX_HEADERS := $(AICP_PROTOCOL_HEADERS) aicp/aicp_posix_stream.h
AICP_SERVICE_CORE := rtos-core/aicp_service.c rtos-core/control_loop.c
AICP_SERVICE_HEADERS := rtos-core/aicp_service.h rtos-core/control_loop.h
AICP_CONTROL_POLICY := ai-model/control_policy.c ai-model/simple_nn.c
AICP_CONTROL_POLICY_HEADERS := ai-model/control_policy.h ai-model/simple_nn.h

.PHONY: protocol-test service-test client-test model-test test reference-server linux-init-aarch64 clean FORCE

$(BIN_DIR):
	mkdir -p $@

$(AARCH64_BIN_DIR):
	mkdir -p $@

$(PROTO_TEST): tests/aicp_protocol_test.c $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ $<

$(SERVICE_TEST): tests/aicp_service_test.c $(AICP_SERVICE_CORE) $(AICP_SERVICE_HEADERS) $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -pthread -o $@ tests/aicp_service_test.c $(AICP_SERVICE_CORE) $(LDLIBS)

$(CLIENT_TEST): tests/aicp_client_test.c aicp/aicp_client.c aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -pthread -o $@ tests/aicp_client_test.c aicp/aicp_client.c

$(MODEL_TEST): tests/aicp_model_test.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) aicp/aicp.h | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ tests/aicp_model_test.c $(AICP_CONTROL_POLICY) $(LDLIBS)

$(REFERENCE_SERVER): host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(AICP_SERVICE_HEADERS) $(AICP_POSIX_HEADERS) | $(BIN_DIR)
	$(CC) $(CPPFLAGS) $(CFLAGS) -o $@ host-tools/aicp_reference_server.c $(AICP_SERVICE_CORE) $(LDLIBS)

$(AARCH64_INIT): FORCE linux-init/aicp_init.c $(AICP_CONTROL_POLICY) $(AICP_CONTROL_POLICY_HEADERS) aicp/aicp_client.c aicp/aicp_client.h $(AICP_POSIX_HEADERS) | $(AARCH64_BIN_DIR)
	$(CROSS_CC) $(CPPFLAGS) $(CFLAGS) -static -o $@ linux-init/aicp_init.c aicp/aicp_client.c $(AICP_CONTROL_POLICY) $(LDLIBS)

FORCE:

protocol-test: $(PROTO_TEST)
	$(PROTO_TEST)

service-test: $(SERVICE_TEST)
	$(SERVICE_TEST)

client-test: $(CLIENT_TEST)
	$(CLIENT_TEST)

model-test: $(MODEL_TEST)
	$(MODEL_TEST)

test: protocol-test service-test client-test model-test

reference-server: $(REFERENCE_SERVER)

linux-init-aarch64: $(AARCH64_INIT)

clean:
	rm -rf $(BIN_DIR)
