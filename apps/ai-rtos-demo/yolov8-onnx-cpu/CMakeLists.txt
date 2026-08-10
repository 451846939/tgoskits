cmake_minimum_required(VERSION 3.16)

project(aicp_yolov8_onnx_cpu C CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)
set(CMAKE_CXX_EXTENSIONS OFF)

if(NOT DEFINED ONNXRUNTIME_ROOT)
  set(ONNXRUNTIME_ROOT "${CMAKE_CURRENT_LIST_DIR}/third_party/onnxruntime")
endif()

add_executable(aicp_yolov8_onnx_cpu
  ../aicp/aicp_client.c
  src/main.cc
)

target_include_directories(aicp_yolov8_onnx_cpu PRIVATE
  "${ONNXRUNTIME_ROOT}/include"
  "${CMAKE_CURRENT_LIST_DIR}/../aicp"
  "${CMAKE_CURRENT_LIST_DIR}/../../starry/orangepi-5-plus-uvc-rknn/rknn-yolov8-image/3rdparty/stb_image"
)

target_link_directories(aicp_yolov8_onnx_cpu PRIVATE
  "${ONNXRUNTIME_ROOT}/lib"
)

target_link_libraries(aicp_yolov8_onnx_cpu PRIVATE onnxruntime m)

set_target_properties(aicp_yolov8_onnx_cpu PROPERTIES
  BUILD_RPATH "$ORIGIN/lib"
  INSTALL_RPATH "$ORIGIN/lib"
)

install(TARGETS aicp_yolov8_onnx_cpu RUNTIME DESTINATION .)
