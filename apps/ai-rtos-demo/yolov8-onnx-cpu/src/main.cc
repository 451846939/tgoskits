// Copyright 2026 The TGOSKits Authors
//
// SPDX-License-Identifier: Apache-2.0

#include <algorithm>
#include <array>
#include <chrono>
#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fstream>
#include <limits>
#include <stdexcept>
#include <string>
#include <vector>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netinet/in.h>
#include <onnxruntime_cxx_api.h>
#include <net/route.h>
#include <sys/select.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <unistd.h>

#define STB_IMAGE_IMPLEMENTATION
#include "stb_image.h"

#include "aicp_client.h"
#include "aicp_posix_stream.h"

struct Options {
    std::string model = "model/yolov8n.onnx";
    std::string labels = "model/coco_80_labels_list.txt";
    std::string image;
    std::string image_list = "validation/images.txt";
    std::string aicp_host = "10.0.3.2";
    std::string client_ip = "10.0.3.3";
    std::string net_prefix = "10.0.3.0";
    std::string netmask = "255.255.255.0";
    std::string iface = "eth0";
    std::string server_mac = "52:54:00:aa:03:02";
    int aicp_port = 8800;
    int input_size = 640;
    int target_class = 32;
    int threads = 1;
    int connect_timeout_ms = 1000;
    int connect_retries = 120;
    int connect_retry_delay_ms = 1000;
    float conf_threshold = 0.25f;
    float nms_threshold = 0.45f;
    bool dry_run = false;
    bool net_config = true;
    bool static_arp = true;
};

struct Image {
    int width = 0;
    int height = 0;
    int channels = 0;
    std::vector<unsigned char> rgb;
};

struct Letterbox {
    float scale = 1.0f;
    float pad_x = 0.0f;
    float pad_y = 0.0f;
};

struct Detection {
    int cls = -1;
    float score = 0.0f;
    float left = 0.0f;
    float top = 0.0f;
    float right = 0.0f;
    float bottom = 0.0f;
};

struct ControlMapping {
    bool has_detection = false;
    int cls = -1;
    float confidence = 0.0f;
    float left = 0.0f;
    float top = 0.0f;
    float right = 0.0f;
    float bottom = 0.0f;
    float target = 0.0f;
    float kp = 0.42f;
    float ki = 0.02f;
    float kd = 0.01f;
    float feed_forward = 0.0f;
    uint32_t mode = 4;
};

static uint64_t monotonic_ns()
{
    auto now = std::chrono::steady_clock::now().time_since_epoch();
    return (uint64_t)std::chrono::duration_cast<std::chrono::nanoseconds>(now).count();
}

static uint64_t client_monotonic_ns(void *)
{
    return monotonic_ns();
}

static float clampf(float value, float low, float high)
{
    if (value < low) return low;
    if (value > high) return high;
    return value;
}

static void idle_if_pid1()
{
    if (getpid() != 1) {
        return;
    }
    std::printf("AICP_YOLO_CPU_IDLE pid=1 reason=linux_init_must_not_exit\n");
    std::fflush(stdout);
    for (;;) {
        pause();
    }
}

static void usage(const char *argv0)
{
    std::printf("Usage: %s --image <jpg> [OPTIONS]\n", argv0);
    std::printf("       %s --image-list <list.txt> [OPTIONS]\n", argv0);
    std::printf("  --model <PATH>              YOLOv8 ONNX model [model/yolov8n.onnx]\n");
    std::printf("  --labels <PATH>             COCO labels file [model/coco_80_labels_list.txt]\n");
    std::printf("  --aicp-host <IPv4>          RTOS AICP server [10.0.3.2]\n");
    std::printf("  --aicp-port <PORT>          RTOS AICP port [8800]\n");
    std::printf("  --client-ip <IPv4>          guest-side static IP [10.0.3.3]\n");
    std::printf("  --net-prefix <IPv4>         connected route prefix [10.0.3.0]\n");
    std::printf("  --netmask <IPv4>            netmask [255.255.255.0]\n");
    std::printf("  --iface <NAME>              network interface [eth0]\n");
    std::printf("  --server-mac <MAC>          RTOS static ARP MAC [52:54:00:aa:03:02]\n");
    std::printf("  --input-size <N>            square model input [640]\n");
    std::printf("  --target-class <ID>         COCO class id, -1 means best object [32]\n");
    std::printf("  --conf <FLOAT>              confidence threshold [0.25]\n");
    std::printf("  --nms <FLOAT>               NMS IoU threshold [0.45]\n");
    std::printf("  --threads <N>               ONNX Runtime intra-op threads [1]\n");
    std::printf("  --connect-timeout-ms <MS>   TCP connect/read/write timeout [1000]\n");
    std::printf("  --connect-retries <N>       retry TCP connect+HELLO before giving up [120]\n");
    std::printf("  --connect-retry-delay-ms <MS> delay between connect retries [1000]\n");
    std::printf("  --dry-run                   run model and mapping without network send\n");
    std::printf("  --no-net-config             skip ioctl interface/route/ARP setup\n");
    std::printf("  --no-static-arp             skip static ARP entry\n");
}

static bool parse_int(const char *name, const char *text, int min_value, int max_value, int *out)
{
    char *end = nullptr;
    errno = 0;
    long v = std::strtol(text, &end, 10);
    if (errno != 0 || end == text || *end != '\0' || v < min_value || v > max_value) {
        std::fprintf(stderr, "invalid %s: %s\n", name, text);
        return false;
    }
    *out = (int)v;
    return true;
}

static bool parse_float(const char *name, const char *text, float min_value, float max_value, float *out)
{
    char *end = nullptr;
    errno = 0;
    float v = std::strtof(text, &end);
    if (errno != 0 || end == text || *end != '\0' || v < min_value || v > max_value) {
        std::fprintf(stderr, "invalid %s: %s\n", name, text);
        return false;
    }
    *out = v;
    return true;
}

static bool parse_args(int argc, char **argv, Options *opt)
{
    for (int i = 1; i < argc; i++) {
        const char *arg = argv[i];
        const char *value = i + 1 < argc ? argv[i + 1] : nullptr;
        if (std::strcmp(arg, "-h") == 0 || std::strcmp(arg, "--help") == 0) {
            usage(argv[0]);
            std::exit(0);
        } else if (std::strcmp(arg, "--model") == 0 && value != nullptr) {
            opt->model = value;
            i++;
        } else if (std::strcmp(arg, "--labels") == 0 && value != nullptr) {
            opt->labels = value;
            i++;
        } else if (std::strcmp(arg, "--image") == 0 && value != nullptr) {
            opt->image = value;
            i++;
        } else if (std::strcmp(arg, "--image-list") == 0 && value != nullptr) {
            opt->image_list = value;
            i++;
        } else if (std::strcmp(arg, "--aicp-host") == 0 && value != nullptr) {
            opt->aicp_host = value;
            i++;
        } else if (std::strcmp(arg, "--aicp-port") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 1, 65535, &opt->aicp_port)) return false;
            i++;
        } else if (std::strcmp(arg, "--client-ip") == 0 && value != nullptr) {
            opt->client_ip = value;
            i++;
        } else if (std::strcmp(arg, "--net-prefix") == 0 && value != nullptr) {
            opt->net_prefix = value;
            i++;
        } else if (std::strcmp(arg, "--netmask") == 0 && value != nullptr) {
            opt->netmask = value;
            i++;
        } else if (std::strcmp(arg, "--iface") == 0 && value != nullptr) {
            opt->iface = value;
            i++;
        } else if (std::strcmp(arg, "--server-mac") == 0 && value != nullptr) {
            opt->server_mac = value;
            i++;
        } else if (std::strcmp(arg, "--input-size") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 32, 4096, &opt->input_size)) return false;
            i++;
        } else if (std::strcmp(arg, "--target-class") == 0 && value != nullptr) {
            if (!parse_int(arg, value, -1, 10000, &opt->target_class)) return false;
            i++;
        } else if (std::strcmp(arg, "--threads") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 1, 64, &opt->threads)) return false;
            i++;
        } else if (std::strcmp(arg, "--connect-timeout-ms") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 1, 60000, &opt->connect_timeout_ms)) return false;
            i++;
        } else if (std::strcmp(arg, "--connect-retries") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 1, 3600, &opt->connect_retries)) return false;
            i++;
        } else if (std::strcmp(arg, "--connect-retry-delay-ms") == 0 && value != nullptr) {
            if (!parse_int(arg, value, 1, 60000, &opt->connect_retry_delay_ms)) return false;
            i++;
        } else if (std::strcmp(arg, "--conf") == 0 && value != nullptr) {
            if (!parse_float(arg, value, 0.001f, 0.999f, &opt->conf_threshold)) return false;
            i++;
        } else if (std::strcmp(arg, "--nms") == 0 && value != nullptr) {
            if (!parse_float(arg, value, 0.001f, 0.999f, &opt->nms_threshold)) return false;
            i++;
        } else if (std::strcmp(arg, "--dry-run") == 0) {
            opt->dry_run = true;
        } else if (std::strcmp(arg, "--no-net-config") == 0) {
            opt->net_config = false;
        } else if (std::strcmp(arg, "--no-static-arp") == 0) {
            opt->static_arp = false;
        } else {
            std::fprintf(stderr, "unknown or incomplete argument: %s\n", arg);
            return false;
        }
    }
    if (!opt->image.empty() && !opt->image_list.empty() && opt->image_list != "validation/images.txt") {
        std::fprintf(stderr, "only one of --image or --image-list can be used\n");
        return false;
    }
    if (!opt->image.empty()) {
        opt->image_list.clear();
    }
    return true;
}

static int set_ifaddr(int ctl, const std::string &ifname, unsigned long request, const std::string &addr)
{
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", ifname.c_str());
    struct sockaddr_in *sin = (struct sockaddr_in *)&ifr.ifr_addr;
    sin->sin_family = AF_INET;
    if (inet_pton(AF_INET, addr.c_str(), &sin->sin_addr) != 1) {
        return -EINVAL;
    }
    if (ioctl(ctl, request, &ifr) != 0) {
        return -errno;
    }
    return 0;
}

static int set_if_up(int ctl, const std::string &ifname)
{
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::snprintf(ifr.ifr_name, sizeof(ifr.ifr_name), "%s", ifname.c_str());
    if (ioctl(ctl, SIOCGIFFLAGS, &ifr) != 0) {
        return -errno;
    }
    ifr.ifr_flags |= IFF_UP;
    if (ioctl(ctl, SIOCSIFFLAGS, &ifr) != 0) {
        return -errno;
    }
    return 0;
}

static int add_connected_route(int ctl, const Options &opt)
{
    struct rtentry route;
    std::memset(&route, 0, sizeof(route));
    struct sockaddr_in *dst = (struct sockaddr_in *)&route.rt_dst;
    struct sockaddr_in *mask = (struct sockaddr_in *)&route.rt_genmask;
    dst->sin_family = AF_INET;
    mask->sin_family = AF_INET;
    if (inet_pton(AF_INET, opt.net_prefix.c_str(), &dst->sin_addr) != 1 ||
        inet_pton(AF_INET, opt.netmask.c_str(), &mask->sin_addr) != 1) {
        return -EINVAL;
    }
    route.rt_flags = RTF_UP;
    route.rt_dev = const_cast<char *>(opt.iface.c_str());
    if (ioctl(ctl, SIOCADDRT, &route) != 0 && errno != EEXIST) {
        return -errno;
    }
    return 0;
}

static int parse_mac(const std::string &text, unsigned char out[6])
{
    unsigned values[6];
    if (std::sscanf(text.c_str(),
                    "%x:%x:%x:%x:%x:%x",
                    &values[0],
                    &values[1],
                    &values[2],
                    &values[3],
                    &values[4],
                    &values[5]) != 6) {
        return -EINVAL;
    }
    for (unsigned i = 0; i < 6; i++) {
        if (values[i] > 0xffu) return -EINVAL;
        out[i] = (unsigned char)values[i];
    }
    return 0;
}

static int add_static_arp(int ctl, const Options &opt)
{
    struct arpreq req;
    unsigned char mac[6];
    int ret = parse_mac(opt.server_mac, mac);
    if (ret != 0) return ret;
    std::memset(&req, 0, sizeof(req));
    struct sockaddr_in *pa = (struct sockaddr_in *)&req.arp_pa;
    pa->sin_family = AF_INET;
    if (inet_pton(AF_INET, opt.aicp_host.c_str(), &pa->sin_addr) != 1) {
        return -EINVAL;
    }
    req.arp_ha.sa_family = ARPHRD_ETHER;
    std::memcpy(req.arp_ha.sa_data, mac, sizeof(mac));
    req.arp_flags = ATF_COM | ATF_PERM;
    std::snprintf(req.arp_dev, sizeof(req.arp_dev), "%s", opt.iface.c_str());
    if (ioctl(ctl, SIOCSARP, &req) != 0) {
        return -errno;
    }
    return 0;
}

static int configure_network(const Options &opt)
{
    if (!opt.net_config) {
        std::printf("AICP_YOLO_CPU_NETCFG skipped=1\n");
        return 0;
    }
    int ctl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctl < 0) return -errno;
    int ret = set_ifaddr(ctl, opt.iface, SIOCSIFADDR, opt.client_ip);
    std::printf("AICP_YOLO_CPU_NETCFG step=SIOCSIFADDR ret=%d iface=%s addr=%s\n", ret, opt.iface.c_str(), opt.client_ip.c_str());
    if (ret == 0) {
        ret = set_ifaddr(ctl, opt.iface, SIOCSIFNETMASK, opt.netmask);
        std::printf("AICP_YOLO_CPU_NETCFG step=SIOCSIFNETMASK ret=%d netmask=%s\n", ret, opt.netmask.c_str());
    }
    if (ret == 0) {
        ret = set_if_up(ctl, opt.iface);
        std::printf("AICP_YOLO_CPU_NETCFG step=SIOCSIFFLAGS ret=%d\n", ret);
    }
    if (ret == 0) {
        ret = add_connected_route(ctl, opt);
        std::printf("AICP_YOLO_CPU_NETCFG step=SIOCADDRT ret=%d prefix=%s\n", ret, opt.net_prefix.c_str());
    }
    if (ret == 0 && opt.static_arp) {
        ret = add_static_arp(ctl, opt);
        std::printf("AICP_YOLO_CPU_NETCFG step=SIOCSARP ret=%d server=%s mac=%s\n", ret, opt.aicp_host.c_str(), opt.server_mac.c_str());
    }
    close(ctl);
    return ret;
}

static std::vector<std::string> load_lines(const std::string &path)
{
    std::ifstream in(path);
    if (!in) {
        throw std::runtime_error("cannot open " + path);
    }
    std::vector<std::string> lines;
    std::string line;
    while (std::getline(in, line)) {
        if (!line.empty() && line.back() == '\r') line.pop_back();
        if (!line.empty() && line[0] != '#') lines.push_back(line);
    }
    return lines;
}

static std::vector<std::string> image_paths(const Options &opt)
{
    if (!opt.image.empty()) return { opt.image };
    return load_lines(opt.image_list);
}

static Image read_image(const std::string &path)
{
    int w = 0;
    int h = 0;
    int c = 0;
    unsigned char *data = stbi_load(path.c_str(), &w, &h, &c, 3);
    if (data == nullptr || w <= 0 || h <= 0) {
        throw std::runtime_error("stbi_load failed: " + path);
    }
    Image image;
    image.width = w;
    image.height = h;
    image.channels = 3;
    image.rgb.assign(data, data + (size_t)w * (size_t)h * 3u);
    stbi_image_free(data);
    return image;
}

static void resize_bilinear_rgb(const Image &src,
                                int dst_w,
                                int dst_h,
                                std::vector<unsigned char> *dst)
{
    dst->assign((size_t)dst_w * (size_t)dst_h * 3u, 114);
    const float scale_x = (float)src.width / (float)dst_w;
    const float scale_y = (float)src.height / (float)dst_h;

    for (int y = 0; y < dst_h; y++) {
        float fy = ((float)y + 0.5f) * scale_y - 0.5f;
        int y0 = std::max(0, (int)std::floor(fy));
        int y1 = std::min(src.height - 1, y0 + 1);
        float wy = fy - (float)y0;
        for (int x = 0; x < dst_w; x++) {
            float fx = ((float)x + 0.5f) * scale_x - 0.5f;
            int x0 = std::max(0, (int)std::floor(fx));
            int x1 = std::min(src.width - 1, x0 + 1);
            float wx = fx - (float)x0;
            for (int ch = 0; ch < 3; ch++) {
                float p00 = src.rgb[((size_t)y0 * src.width + x0) * 3u + ch];
                float p01 = src.rgb[((size_t)y0 * src.width + x1) * 3u + ch];
                float p10 = src.rgb[((size_t)y1 * src.width + x0) * 3u + ch];
                float p11 = src.rgb[((size_t)y1 * src.width + x1) * 3u + ch];
                float p0 = p00 + (p01 - p00) * wx;
                float p1 = p10 + (p11 - p10) * wx;
                (*dst)[((size_t)y * dst_w + x) * 3u + ch] = (unsigned char)clampf(p0 + (p1 - p0) * wy, 0.0f, 255.0f);
            }
        }
    }
}

static std::vector<float> preprocess(const Image &image, int input_size, Letterbox *box)
{
    float scale = std::min((float)input_size / (float)image.width, (float)input_size / (float)image.height);
    int resized_w = std::max(1, (int)std::round((float)image.width * scale));
    int resized_h = std::max(1, (int)std::round((float)image.height * scale));
    int pad_x = (input_size - resized_w) / 2;
    int pad_y = (input_size - resized_h) / 2;

    std::vector<unsigned char> resized;
    resize_bilinear_rgb(image, resized_w, resized_h, &resized);

    std::vector<float> chw((size_t)3 * input_size * input_size, 114.0f / 255.0f);
    for (int y = 0; y < resized_h; y++) {
        for (int x = 0; x < resized_w; x++) {
            for (int ch = 0; ch < 3; ch++) {
                size_t src_idx = ((size_t)y * resized_w + x) * 3u + ch;
                size_t dst_idx = (size_t)ch * input_size * input_size + (size_t)(y + pad_y) * input_size + (x + pad_x);
                chw[dst_idx] = (float)resized[src_idx] / 255.0f;
            }
        }
    }

    box->scale = scale;
    box->pad_x = (float)pad_x;
    box->pad_y = (float)pad_y;
    return chw;
}

static float iou(const Detection &a, const Detection &b)
{
    float left = std::max(a.left, b.left);
    float top = std::max(a.top, b.top);
    float right = std::min(a.right, b.right);
    float bottom = std::min(a.bottom, b.bottom);
    float inter = std::max(0.0f, right - left) * std::max(0.0f, bottom - top);
    float area_a = std::max(0.0f, a.right - a.left) * std::max(0.0f, a.bottom - a.top);
    float area_b = std::max(0.0f, b.right - b.left) * std::max(0.0f, b.bottom - b.top);
    return inter / std::max(area_a + area_b - inter, 1e-6f);
}

static std::vector<Detection> nms(std::vector<Detection> detections, float threshold)
{
    std::sort(detections.begin(), detections.end(), [](const Detection &a, const Detection &b) {
        return a.score > b.score;
    });
    std::vector<Detection> kept;
    std::vector<char> removed(detections.size(), 0);
    for (size_t i = 0; i < detections.size(); i++) {
        if (removed[i]) continue;
        kept.push_back(detections[i]);
        for (size_t j = i + 1; j < detections.size(); j++) {
            if (!removed[j] && detections[i].cls == detections[j].cls && iou(detections[i], detections[j]) > threshold) {
                removed[j] = 1;
            }
        }
    }
    return kept;
}

static std::vector<Detection> decode_yolov8(const float *data,
                                            const std::vector<int64_t> &shape,
                                            const Letterbox &box,
                                            const Image &image,
                                            const Options &opt)
{
    if (shape.size() != 3 || shape[0] != 1) {
        throw std::runtime_error("unsupported YOLOv8 output shape");
    }

    bool channels_first = shape[1] < shape[2];
    int channels = (int)(channels_first ? shape[1] : shape[2]);
    int count = (int)(channels_first ? shape[2] : shape[1]);
    if (channels < 84 || count <= 0) {
        throw std::runtime_error("unsupported YOLOv8 output channel count");
    }
    bool has_objectness = channels >= 85;
    int class_offset = has_objectness ? 5 : 4;
    int class_count = channels - class_offset;

    auto value_at = [&](int row, int ch) -> float {
        if (channels_first) {
            return data[(size_t)ch * count + row];
        }
        return data[(size_t)row * channels + ch];
    };

    std::vector<Detection> out;
    for (int i = 0; i < count; i++) {
        float obj = has_objectness ? value_at(i, 4) : 1.0f;
        int best_cls = -1;
        float best_score = -std::numeric_limits<float>::infinity();
        for (int cls = 0; cls < class_count; cls++) {
            float score = obj * value_at(i, class_offset + cls);
            if (score > best_score) {
                best_score = score;
                best_cls = cls;
            }
        }
        if (best_score < opt.conf_threshold) continue;
        if (opt.target_class >= 0 && best_cls != opt.target_class) continue;

        float cx = value_at(i, 0);
        float cy = value_at(i, 1);
        float w = value_at(i, 2);
        float h = value_at(i, 3);
        Detection det;
        det.cls = best_cls;
        det.score = best_score;
        det.left = clampf((cx - w * 0.5f - box.pad_x) / box.scale, 0.0f, (float)image.width);
        det.top = clampf((cy - h * 0.5f - box.pad_y) / box.scale, 0.0f, (float)image.height);
        det.right = clampf((cx + w * 0.5f - box.pad_x) / box.scale, 0.0f, (float)image.width);
        det.bottom = clampf((cy + h * 0.5f - box.pad_y) / box.scale, 0.0f, (float)image.height);
        out.push_back(det);
    }

    return nms(std::move(out), opt.nms_threshold);
}

static ControlMapping map_detection_to_control(const std::vector<Detection> &detections, const Image &image)
{
    ControlMapping mapping;
    if (detections.empty()) {
        return mapping;
    }
    const Detection &best = detections.front();
    float width = std::max(1.0f, best.right - best.left);
    float height = std::max(1.0f, best.bottom - best.top);
    float center_x = best.left + width * 0.5f;
    float center_y = best.top + height * 0.5f;
    float x_error = center_x / (float)image.width - 0.5f;
    float y_error = 0.5f - center_y / (float)image.height;
    float area_ratio = clampf((width * height) / ((float)image.width * (float)image.height), 0.0f, 1.0f);
    float conf = clampf(best.score, 0.0f, 1.0f);

    mapping.has_detection = true;
    mapping.cls = best.cls;
    mapping.confidence = conf;
    mapping.left = best.left;
    mapping.top = best.top;
    mapping.right = best.right;
    mapping.bottom = best.bottom;
    mapping.target = clampf(x_error * 2.0f, -1.0f, 1.0f);
    mapping.kp = 0.45f + 0.35f * conf;
    mapping.ki = 0.02f + 0.10f * area_ratio;
    mapping.kd = 0.02f + 0.10f * std::fabs(x_error);
    mapping.feed_forward = clampf(y_error * 0.35f, -0.25f, 0.25f);
    mapping.mode = 4;
    return mapping;
}

static int set_io_timeout(int fd, int timeout_ms)
{
    struct timeval tv;
    tv.tv_sec = timeout_ms / 1000;
    tv.tv_usec = (timeout_ms % 1000) * 1000;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) != 0) return -errno;
    if (setsockopt(fd, SOL_SOCKET, SO_SNDTIMEO, &tv, sizeof(tv)) != 0) return -errno;
    return 0;
}

static int connect_with_timeout(int fd, const struct sockaddr *addr, socklen_t addr_len, int timeout_ms)
{
    int flags = fcntl(fd, F_GETFL, 0);
    if (flags < 0) return -errno;
    if (fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0) return -errno;
    int ret = connect(fd, addr, addr_len);
    if (ret != 0 && errno != EINPROGRESS) {
        int saved = errno;
        (void)fcntl(fd, F_SETFL, flags);
        return -saved;
    }
    if (ret != 0) {
        fd_set wfds;
        FD_ZERO(&wfds);
        FD_SET(fd, &wfds);
        struct timeval tv;
        tv.tv_sec = timeout_ms / 1000;
        tv.tv_usec = (timeout_ms % 1000) * 1000;
        ret = select(fd + 1, nullptr, &wfds, nullptr, &tv);
        if (ret == 0) {
            (void)fcntl(fd, F_SETFL, flags);
            return -ETIMEDOUT;
        }
        if (ret < 0) {
            int saved = errno;
            (void)fcntl(fd, F_SETFL, flags);
            return -saved;
        }
        int so_error = 0;
        socklen_t len = sizeof(so_error);
        if (getsockopt(fd, SOL_SOCKET, SO_ERROR, &so_error, &len) != 0) {
            int saved = errno;
            (void)fcntl(fd, F_SETFL, flags);
            return -saved;
        }
        if (so_error != 0) {
            (void)fcntl(fd, F_SETFL, flags);
            return -so_error;
        }
    }
    if (fcntl(fd, F_SETFL, flags) != 0) return -errno;
    return set_io_timeout(fd, timeout_ms);
}

class AicpClient {
public:
    ~AicpClient() { close_fd(); }

    int connect_to(const std::string &host, int port, int timeout_ms)
    {
        close_fd();
        fd_ = socket(AF_INET, SOCK_STREAM, 0);
        if (fd_ < 0) return -errno;
        struct sockaddr_in addr;
        std::memset(&addr, 0, sizeof(addr));
        addr.sin_family = AF_INET;
        addr.sin_port = htons((uint16_t)port);
        if (inet_pton(AF_INET, host.c_str(), &addr.sin_addr) != 1) {
            close_fd();
            return -EINVAL;
        }
        int ret = connect_with_timeout(fd_, (struct sockaddr *)&addr, sizeof(addr), timeout_ms);
        if (ret != 0) {
            close_fd();
        } else {
            aicp_posix_stream_init(&stream_, fd_);
        }
        return ret;
    }

    int send_hello()
    {
        const char payload[] = "{\"role\":\"yolov8-onnx-cpu\",\"model\":\"yolov8n.onnx\",\"cap\":\"control,status\"}";
        return aicp_client_session_send_hello(
            &stream_.stream,
            &seq_,
            payload,
            (uint32_t)sizeof(payload),
            &client_ops_);
    }

    int send_control(const ControlMapping &mapping, uint64_t *rtt_ns, float *measured, float *error, float *output)
    {
        struct aicp_control_payload control;
        control.target = mapping.target;
        control.kp = mapping.kp;
        control.ki = mapping.ki;
        control.kd = mapping.kd;
        control.feed_forward = mapping.feed_forward;
        control.mode = mapping.mode;

        struct aicp_status_payload status;
        int ret = aicp_client_session_transact_control(
            &stream_.stream,
            &seq_,
            &control,
            &status,
            rtt_ns,
            &client_ops_);
        if (ret != 0) return ret;
        *measured = status.measured;
        *error = status.error;
        *output = status.control_output;
        return 0;
    }

private:
    void close_fd()
    {
        if (fd_ >= 0) {
            close(fd_);
            fd_ = -1;
        }
    }

    int fd_ = -1;
    struct aicp_posix_stream stream_ = {};
    uint32_t seq_ = 1;
    const struct aicp_client_ops client_ops_ = {
        client_monotonic_ns,
        nullptr,
        nullptr,
    };
};

static std::vector<std::string> read_labels(const std::string &path)
{
    try {
        return load_lines(path);
    } catch (const std::exception &) {
        return {};
    }
}

int main(int argc, char **argv)
{
    Options opt;
    if (!parse_args(argc, argv, &opt)) {
        usage(argv[0]);
        return 2;
    }

    try {
        std::vector<std::string> paths = image_paths(opt);
        std::vector<std::string> labels = read_labels(opt.labels);
        std::printf("AICP_YOLO_CPU_BEGIN model=%s images=%llu host=%s port=%d dry_run=%d target_class=%d backend=onnxruntime-cpu\n",
                    opt.model.c_str(),
                    (unsigned long long)paths.size(),
                    opt.aicp_host.c_str(),
                    opt.aicp_port,
                    opt.dry_run ? 1 : 0,
                    opt.target_class);

        if (!opt.dry_run) {
            int net_ret = configure_network(opt);
            if (net_ret != 0) {
                std::printf("AICP_YOLO_CPU_FAIL stage=netcfg ret=%d\n", net_ret);
                return 1;
            }
        }

        Ort::Env env(ORT_LOGGING_LEVEL_WARNING, "aicp-yolov8-onnx-cpu");
        Ort::SessionOptions session_options;
        session_options.SetIntraOpNumThreads(opt.threads);
        session_options.SetGraphOptimizationLevel(GraphOptimizationLevel::ORT_ENABLE_EXTENDED);
        Ort::Session session(env, opt.model.c_str(), session_options);
        Ort::AllocatorWithDefaultOptions allocator;
        auto input_name_alloc = session.GetInputNameAllocated(0, allocator);
        auto output_name_alloc = session.GetOutputNameAllocated(0, allocator);
        std::string input_name = input_name_alloc.get();
        std::string output_name = output_name_alloc.get();
        const char *input_names[] = { input_name.c_str() };
        const char *output_names[] = { output_name.c_str() };

        std::array<int64_t, 4> input_shape = { 1, 3, opt.input_size, opt.input_size };
        Ort::MemoryInfo memory_info = Ort::MemoryInfo::CreateCpu(OrtArenaAllocator, OrtMemTypeDefault);

        AicpClient client;
        if (!opt.dry_run) {
            int ret = -ECONNREFUSED;
            for (int attempt = 1; attempt <= opt.connect_retries; attempt++) {
                ret = client.connect_to(opt.aicp_host, opt.aicp_port, opt.connect_timeout_ms);
                if (ret == 0) {
                    ret = client.send_hello();
                    if (ret == 0) {
                        std::printf("AICP_YOLO_CPU_CONNECTED attempt=%d\n", attempt);
                        break;
                    }
                    std::printf("AICP_YOLO_CPU_CONNECT_RETRY attempt=%d stage=hello ret=%d\n", attempt, ret);
                } else {
                    std::printf("AICP_YOLO_CPU_CONNECT_RETRY attempt=%d stage=connect ret=%d\n", attempt, ret);
                }
                if (attempt < opt.connect_retries) {
                    usleep((useconds_t)opt.connect_retry_delay_ms * 1000);
                }
            }
            if (ret != 0) {
                std::printf("AICP_YOLO_CPU_FAIL stage=connect_or_hello ret=%d retries=%d\n", ret, opt.connect_retries);
                return 1;
            }
        }

        unsigned ok = 0;
        unsigned failed = 0;
        uint64_t infer_sum_ns = 0;
        uint64_t infer_max_ns = 0;
        uint64_t rtt_sum_ns = 0;
        uint64_t rtt_max_ns = 0;

        for (const std::string &path : paths) {
            try {
                Image image = read_image(path);
                Letterbox box;
                std::vector<float> input = preprocess(image, opt.input_size, &box);
                Ort::Value input_tensor = Ort::Value::CreateTensor<float>(
                    memory_info, input.data(), input.size(), input_shape.data(), input_shape.size());

                uint64_t infer_begin = monotonic_ns();
                auto outputs = session.Run(Ort::RunOptions{ nullptr },
                                           input_names,
                                           &input_tensor,
                                           1,
                                           output_names,
                                           1);
                uint64_t infer_ns = monotonic_ns() - infer_begin;
                infer_sum_ns += infer_ns;
                infer_max_ns = std::max(infer_max_ns, infer_ns);

                auto output_info = outputs.front().GetTensorTypeAndShapeInfo();
                std::vector<int64_t> output_shape = output_info.GetShape();
                const float *output_data = outputs.front().GetTensorData<float>();
                std::vector<Detection> detections = decode_yolov8(output_data, output_shape, box, image, opt);
                ControlMapping mapping = map_detection_to_control(detections, image);
                std::string label = mapping.cls >= 0 && mapping.cls < (int)labels.size() ? labels[(size_t)mapping.cls] : "";

                std::printf("AICP_YOLO_CPU_RESULT image=%s detections=%llu selected=%d cls=%d label=%s score=%.3f box=%.1f,%.1f,%.1f,%.1f target=%.4f kp=%.4f ki=%.4f kd=%.4f feed_forward=%.4f mode=%u infer_ns=%llu\n",
                            path.c_str(),
                            (unsigned long long)detections.size(),
                            mapping.has_detection ? 1 : 0,
                            mapping.cls,
                            label.c_str(),
                            mapping.confidence,
                            mapping.left,
                            mapping.top,
                            mapping.right,
                            mapping.bottom,
                            mapping.target,
                            mapping.kp,
                            mapping.ki,
                            mapping.kd,
                            mapping.feed_forward,
                            mapping.mode,
                            (unsigned long long)infer_ns);

                if (!opt.dry_run) {
                    uint64_t rtt_ns = 0;
                    float measured = 0.0f;
                    float error = 0.0f;
                    float output = 0.0f;
                    int ret = client.send_control(mapping, &rtt_ns, &measured, &error, &output);
                    if (ret != 0) {
                        std::printf("AICP_YOLO_CPU_FAIL image=%s stage=aicp_send ret=%d\n", path.c_str(), ret);
                        failed++;
                        continue;
                    }
                    rtt_sum_ns += rtt_ns;
                    rtt_max_ns = std::max(rtt_max_ns, rtt_ns);
                    std::printf("AICP_YOLO_CPU_CONTROL image=%s rtt_ns=%llu measured=%.4f error=%.4f output=%.4f\n",
                                path.c_str(),
                                (unsigned long long)rtt_ns,
                                measured,
                                error,
                                output);
                }
                ok++;
            } catch (const std::exception &e) {
                std::printf("AICP_YOLO_CPU_FAIL image=%s reason=%s\n", path.c_str(), e.what());
                failed++;
            }
        }

        unsigned denom = ok == 0 ? 1 : ok;
        std::printf("AICP_YOLO_CPU_DONE ok=%u failed=%u avg_infer_ns=%llu max_infer_ns=%llu avg_rtt_ns=%llu max_rtt_ns=%llu\n",
                    ok,
                    failed,
                    (unsigned long long)(infer_sum_ns / denom),
                    (unsigned long long)infer_max_ns,
                    (unsigned long long)(rtt_sum_ns / denom),
                    (unsigned long long)rtt_max_ns);
        idle_if_pid1();
        return failed == 0 ? 0 : 1;
    } catch (const std::exception &e) {
        std::printf("AICP_YOLO_CPU_FATAL reason=%s\n", e.what());
        return 1;
    }
}
