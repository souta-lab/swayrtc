// SwayRTC zero-copy capture: wlr-screencopy into a dma-buf, AMD VPP converts it
// to NV12, Intel encodes it. One process, no CPU pixel work.
//
// The compositor renders on the AMD GPU (that is where the monitor hangs), so
// the captured buffer is an AMD dma-buf. Only AMD exposes a video processor on
// this machine and only Intel exposes an H.264 encoder, hence the split:
//
//   screencopy -> ARGB dma-buf (AMD) -> VPP -> linear NV12 (shared) -> h264_vaapi (Intel)
//
// The NV12 buffer is allocated on the Intel device because i915 can mmap linear
// buffers and AMD can import them; the reverse is not true.
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <gbm.h>
#include <libdrm/drm_fourcc.h>
#include <wayland-client.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <time.h>

#include "proto/linux-dmabuf-client.h"
#include "proto/screencopy-client.h"

#ifndef VA_FOURCC_GR88
#define VA_FOURCC_GR88 VA_FOURCC('G', 'R', '8', '8')
#endif

static struct wl_display *display;
static struct wl_registry *registry;
static struct zwlr_screencopy_manager_v1 *screencopy;
static struct zwp_linux_dmabuf_v1 *dmabuf;
static struct wl_output *output;
static char want_output[64] = "HEADLESS-1";
static int output_found = 0;

static int frame_ready = 0, frame_failed = 0;
static uint32_t dmabuf_format = 0, dmabuf_w = 0, dmabuf_h = 0;
static int have_dmabuf_info = 0;

// modifiers advertised for the capture format
static uint64_t mods[64];
static int nmods = 0;

static VADisplay amd_dpy, intel_dpy;
static int amd_fd = -1, intel_fd = -1;
static struct gbm_device *amd_gbm, *intel_gbm;
static struct gbm_bo *nv12_bo;
static VASurfaceID nv12_surf;      // AMD view of the shared buffer
static VASurfaceID nv12_surf_intel; // Intel view of the same buffer
static unsigned nv12_stride;
static VAConfigID vpp_cfg;
static VAContextID vpp_ctx;
static VABufferID vpp_buf;
static AVCodecContext *enc;
static AVBufferRef *enc_dev;
static int g_w, g_h;

static double now_ms(void){struct timespec ts;clock_gettime(CLOCK_MONOTONIC,&ts);return ts.tv_sec*1000.0+ts.tv_nsec/1e6;}

static void die(const char *what) {
    fprintf(stderr, "zerocopy-capture: %s\n", what);
    exit(1);
}

/* ---------------------------------------------------------------- wayland */

static void on_output_geometry(void *d, struct wl_output *o, int32_t x, int32_t y, int32_t pw, int32_t ph,
                            int32_t sub, const char *make, const char *model, int32_t tr) {}
static void on_output_mode(void *d, struct wl_output *o, uint32_t flags, int32_t w, int32_t h, int32_t r) {}
static void on_output_done(void *d, struct wl_output *o) {}
static void on_output_scale(void *d, struct wl_output *o, int32_t f) {}
static void on_output_name(void *d, struct wl_output *o, const char *name) {
    if (strcmp(name, want_output) == 0) output_found = 1;
}
static void on_output_description(void *d, struct wl_output *o, const char *desc) {}
static const struct wl_output_listener output_listener = {
    on_output_geometry, on_output_mode, on_output_done, on_output_scale, on_output_name, on_output_description,
};

static void on_dmabuf_format(void *d, struct zwp_linux_dmabuf_v1 *z, uint32_t format) {}
static void on_dmabuf_modifier(void *d, struct zwp_linux_dmabuf_v1 *z, uint32_t format,
                            uint32_t hi, uint32_t lo) {
    if (format == dmabuf_format || dmabuf_format == 0) {
        if (nmods < 64) mods[nmods++] = ((uint64_t)hi << 32) | lo;
    }
}
static const struct zwp_linux_dmabuf_v1_listener dmabuf_listener = { on_dmabuf_format, on_dmabuf_modifier };

static void on_frame_buffer(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t format,
                         uint32_t w, uint32_t h, uint32_t stride) {}
static void on_frame_flags(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t flags) {}
static void on_frame_ready(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t hi, uint32_t lo, uint32_t ns) {
    frame_ready = 1;
}
static void on_frame_failed(void *d, struct zwlr_screencopy_frame_v1 *f) { frame_failed = 1; }
static void on_frame_damage(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t x, uint32_t y, uint32_t w, uint32_t h) {}
static void on_frame_linux_dmabuf(void *d, struct zwlr_screencopy_frame_v1 *f, uint32_t format,
                               uint32_t w, uint32_t h) {
    dmabuf_format = format;
    dmabuf_w = w;
    dmabuf_h = h;
    have_dmabuf_info = 1;
    fprintf(stderr, "frame linux_dmabuf: format=0x%x (%c%c%c%c) %ux%u\n", format,
            format & 0xff, (format >> 8) & 0xff, (format >> 16) & 0xff, (format >> 24) & 0xff, w, h);
}
static void on_frame_buffer_done(void *d, struct zwlr_screencopy_frame_v1 *f) {}
static const struct zwlr_screencopy_frame_v1_listener frame_listener = {
    on_frame_buffer, on_frame_flags, on_frame_ready, on_frame_failed, on_frame_damage,
    on_frame_linux_dmabuf, on_frame_buffer_done,
};

static void registry_global(void *d, struct wl_registry *r, uint32_t name, const char *iface, uint32_t version) {
    if (strcmp(iface, zwlr_screencopy_manager_v1_interface.name) == 0) {
        screencopy = wl_registry_bind(r, name, &zwlr_screencopy_manager_v1_interface, version < 3 ? version : 3);
    } else if (strcmp(iface, zwp_linux_dmabuf_v1_interface.name) == 0) {
        dmabuf = wl_registry_bind(r, name, &zwp_linux_dmabuf_v1_interface, version < 3 ? version : 3);
        zwp_linux_dmabuf_v1_add_listener(dmabuf, &dmabuf_listener, NULL);
    } else if (strcmp(iface, wl_output_interface.name) == 0) {
        struct wl_output *o = wl_registry_bind(r, name, &wl_output_interface, version < 4 ? version : 4);
        wl_output_add_listener(o, &output_listener, NULL);
        if (!output) output = o;
    }
}
static void registry_remove(void *d, struct wl_registry *r, uint32_t name) {}
static const struct wl_registry_listener registry_listener = { registry_global, registry_remove };

/* ------------------------------------------------------------------- gpu */

static VADisplay open_va(const char *node, int *fd_out) {
    int fd = open(node, O_RDWR);
    if (fd < 0) return NULL;
    VADisplay d = vaGetDisplayDRM(fd);
    int major, minor;
    if (vaInitialize(d, &major, &minor) != VA_STATUS_SUCCESS) return NULL;
    *fd_out = fd;
    return d;
}

static VASurfaceID import_argb(int fd, unsigned stride, uint64_t modifier, int w, int h) {
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_BGRX;
    desc.width = w;
    desc.height = h;
    desc.num_objects = 1;
    desc.objects[0].fd = fd;
    desc.objects[0].size = (size_t)stride * h;
    desc.objects[0].drm_format_modifier = modifier;
    desc.num_layers = 1;
    desc.layers[0].drm_format = VA_FOURCC_BGRX;
    desc.layers[0].num_planes = 1;
    desc.layers[0].offset[0] = 0;
    desc.layers[0].pitch[0] = stride;
    desc.layers[0].object_index[0] = 0;
    VASurfaceAttrib at[2];
    memset(at, 0, sizeof(at));
    at[0].type = VASurfaceAttribMemoryType;
    at[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    at[0].value.type = VAGenericValueTypeInteger;
    at[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    at[1].type = VASurfaceAttribExternalBufferDescriptor;
    at[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    at[1].value.type = VAGenericValueTypePointer;
    at[1].value.value.p = &desc;
    VASurfaceID surf;
    if (vaCreateSurfaces(amd_dpy, VA_RT_FORMAT_RGB32, w, h, &surf, 1, at, 2) != VA_STATUS_SUCCESS)
        return VA_INVALID_SURFACE;
    return surf;
}

static void setup_nv12(int w, int h) {
    // Allocate on the AMD device: it is the one that writes it (VPP), and both
    // drivers import a linear buffer allocated there.
    nv12_bo = gbm_bo_create(amd_gbm, w, h * 3 / 2, GBM_FORMAT_R8, GBM_BO_USE_LINEAR);
    if (!nv12_bo) die("cannot allocate the NV12 buffer");
    nv12_stride = gbm_bo_get_stride(nv12_bo);
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_NV12;
    desc.width = w;
    desc.height = h;
    desc.num_objects = 1;
    desc.objects[0].fd = gbm_bo_get_fd(nv12_bo);
    desc.objects[0].size = (size_t)nv12_stride * h * 3 / 2;
    desc.objects[0].drm_format_modifier = DRM_FORMAT_MOD_LINEAR;
    desc.num_layers = 2;
    desc.layers[0].drm_format = VA_FOURCC_Y8;
    desc.layers[0].num_planes = 1;
    desc.layers[0].offset[0] = 0;
    desc.layers[0].pitch[0] = nv12_stride;
    desc.layers[0].object_index[0] = 0;
    desc.layers[1].drm_format = VA_FOURCC_GR88;
    desc.layers[1].num_planes = 1;
    desc.layers[1].offset[0] = (size_t)nv12_stride * h;
    desc.layers[1].pitch[0] = nv12_stride;
    desc.layers[1].object_index[0] = 0;
    VASurfaceAttrib at[2];
    memset(at, 0, sizeof(at));
    at[0].type = VASurfaceAttribMemoryType;
    at[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    at[0].value.type = VAGenericValueTypeInteger;
    at[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    at[1].type = VASurfaceAttribExternalBufferDescriptor;
    at[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    at[1].value.type = VAGenericValueTypePointer;
    at[1].value.value.p = &desc;
    if (vaCreateSurfaces(amd_dpy, VA_RT_FORMAT_YUV420, w, h, &nv12_surf, 1, at, 2) != VA_STATUS_SUCCESS)
        die("AMD cannot import the NV12 buffer");
    if (vaCreateSurfaces(intel_dpy, VA_RT_FORMAT_YUV420, w, h, &nv12_surf_intel, 1, at, 2) != VA_STATUS_SUCCESS)
        die("Intel cannot import the NV12 buffer");
    if (vaCreateConfig(amd_dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vpp_cfg) != VA_STATUS_SUCCESS)
        die("no VPP config");
    VASurfaceID surfaces[1] = { nv12_surf };
    if (vaCreateContext(amd_dpy, vpp_cfg, w, h, VA_PROGRESSIVE, surfaces, 1, &vpp_ctx) != VA_STATUS_SUCCESS)
        die("no VPP context");
    if (vaCreateBuffer(amd_dpy, vpp_ctx, VAProcPipelineParameterBufferType,
                       sizeof(VAProcPipelineParameterBuffer), 1, NULL, &vpp_buf) != VA_STATUS_SUCCESS)
        die("no VPP buffer");
}

static void setup_encoder(int w, int h, int qp, int gop, int kbps) {
    enc_dev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_VAAPI);
    if (!enc_dev) die("no hw device");
    AVHWDeviceContext *dc = (AVHWDeviceContext *)enc_dev->data;
    AVVAAPIDeviceContext *vc = (AVVAAPIDeviceContext *)dc->hwctx;
    vc->display = intel_dpy;
    if (av_hwdevice_ctx_init(enc_dev) < 0) die("hw device init failed");
    const AVCodec *codec = avcodec_find_encoder_by_name("h264_vaapi");
    if (!codec) die("h264_vaapi missing");
    enc = avcodec_alloc_context3(codec);
    enc->width = w;
    enc->height = h;
    enc->time_base = (AVRational){ 1, 60 };
    enc->framerate = (AVRational){ 60, 1 };
    enc->pix_fmt = AV_PIX_FMT_VAAPI;
    enc->profile = AV_PROFILE_H264_CONSTRAINED_BASELINE;
    enc->gop_size = gop;
    enc->max_b_frames = 0;
    enc->bit_rate = (int64_t)kbps * 1000;
    // Constant bitrate with a small VBV, like a game streaming host: a single
    // frame must fit inside the link budget, otherwise one full-screen change
    // (measured 190 KB) occupies the uplink for ~250 ms and every frame behind
    // it queues up. CQP let frames grow without bound.
    // CQP keeps a constant quality (text stays sharp); the per-frame cap keeps
    // one big change from occupying the uplink.
    // VBR by default: CQP gives the best quality but cannot bound a frame, and
    // an unbounded frame (measured 190 KB) occupies the uplink for ~250 ms.
    const char *rc = getenv("SWAYRTC_RC") ? getenv("SWAYRTC_RC") : "vbr";
    int is_vbr = strcmp(rc, "cqp") != 0;
    av_opt_set(enc->priv_data, "rc_mode", is_vbr ? "VBR" : "CQP", 0);
    av_opt_set_int(enc->priv_data, "qp", qp, 0);
    // rc_max_rate / rc_buffer_size are AVCodecContext fields, not private
    // options; max_frame_size is the hard per-frame cap.
    int vbv = getenv("SWAYRTC_VBV") ? atoi(getenv("SWAYRTC_VBV")) : 500000;
    if (is_vbr) {
        // These are rejected outright in CQP mode ("Max frame size is invalid
        // in CQP rate control mode"), which used to make the encoder fail to
        // open and left the viewer with no video at all.
        enc->rc_max_rate = (int64_t)kbps * 1000;
        enc->rc_buffer_size = vbv;
        av_opt_set_int(enc->priv_data, "max_frame_size", vbv, 0);
    }
    av_opt_set_int(enc->priv_data, "async_depth", 1, 0);

    enc->hw_device_ctx = av_buffer_ref(enc_dev);
    AVBufferRef *fc_ref = av_hwframe_ctx_alloc(enc_dev);
    AVHWFramesContext *fc = (AVHWFramesContext *)fc_ref->data;
    fc->format = AV_PIX_FMT_VAAPI;
    fc->sw_format = AV_PIX_FMT_NV12;
    fc->width = w;
    fc->height = h;
    fc->initial_pool_size = 4;
    if (av_hwframe_ctx_init(fc_ref) < 0) die("frames context failed");
    enc->hw_frames_ctx = av_buffer_ref(fc_ref);
    if (avcodec_open2(enc, codec, NULL) < 0) die("encoder open failed");
}

static void drain(void) {
    AVPacket *pkt = av_packet_alloc();
    while (avcodec_receive_packet(enc, pkt) == 0) {
        fwrite(pkt->data, 1, pkt->size, stdout);
        // Trailing access unit delimiter: the host's Annex-B parser can only
        // close an access unit when the next one starts, which costs a whole
        // frame. An AUD right after the slices closes it immediately.
        static const unsigned char aud[6] = { 0, 0, 0, 1, 0x09, 0xf0 };
        fwrite(aud, 1, sizeof(aud), stdout);
        if (getenv("SWAYRTC_TIMING")) {
            struct timespec ts;
            clock_gettime(CLOCK_REALTIME, &ts);
            fprintf(stderr, "packet ts=%lld size=%d\n",
                    (long long)ts.tv_sec * 1000 + ts.tv_nsec / 1000000, pkt->size);
        }
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
}

static void encode_surface(VASurfaceID src, int w, int h, int64_t pts) {
    VAProcPipelineParameterBuffer *pl = NULL;
    if (vaMapBuffer(amd_dpy, vpp_buf, (void **)&pl) != VA_STATUS_SUCCESS) return;
    memset(pl, 0, sizeof(*pl));
    VARectangle sr = { 0, 0, w, h }, dr = { 0, 0, w, h };
    pl->surface = src;
    pl->surface_region = &sr;
    pl->output_region = &dr;
    pl->output_background_color = 0xff000000;
    pl->filter_flags = VA_FILTER_SCALING_DEFAULT;
    pl->surface_color_standard = VAProcColorStandardSRGB;
    pl->output_color_standard = VAProcColorStandardBT709;
    vaUnmapBuffer(amd_dpy, vpp_buf);
    VAStatus st = vaBeginPicture(amd_dpy, vpp_ctx, nv12_surf);
    if (st != VA_STATUS_SUCCESS) { fprintf(stderr, "vpp begin: %s\n", vaErrorStr(st)); return; }
    st = vaRenderPicture(amd_dpy, vpp_ctx, &vpp_buf, 1);
    if (st != VA_STATUS_SUCCESS) fprintf(stderr, "vpp render: %s\n", vaErrorStr(st));
    st = vaEndPicture(amd_dpy, vpp_ctx);
    if (st != VA_STATUS_SUCCESS) fprintf(stderr, "vpp end: %s\n", vaErrorStr(st));
    st = vaSyncSurface(amd_dpy, nv12_surf);
    if (st != VA_STATUS_SUCCESS) fprintf(stderr, "vpp sync: %s\n", vaErrorStr(st));

    // Hand the imported surface straight to the encoder: no copy at all. The
    // dummy buffer keeps ffmpeg happy (it expects a reference on the frame).
    AVFrame *f = av_frame_alloc();
    f->format = AV_PIX_FMT_VAAPI;
    f->width = w;
    f->height = h;
    f->data[3] = (uint8_t *)(uintptr_t)nv12_surf_intel;
    f->buf[0] = av_buffer_alloc(1);
    f->hw_frames_ctx = av_buffer_ref(enc->hw_frames_ctx);
    f->pts = pts;
    double t_send = now_ms();
    if (avcodec_send_frame(enc, f) >= 0) drain();
    if (getenv("SWAYRTC_TIMING") && pts % 30 == 0) {
        fprintf(stderr, "enc: send->packet %.1f ms\n", now_ms() - t_send);
    }
    av_frame_free(&f);
}

/* ------------------------------------------------------------------ main */

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    if (getenv("SWAYRTC_OUTPUT")) snprintf(want_output, sizeof(want_output), "%s", getenv("SWAYRTC_OUTPUT"));
    int qp = getenv("SWAYRTC_QP") ? atoi(getenv("SWAYRTC_QP")) : 18;
    int gop = getenv("SWAYRTC_GOP") ? atoi(getenv("SWAYRTC_GOP")) : 60;
    int kbps = getenv("SWAYRTC_BITRATE_KBPS") ? atoi(getenv("SWAYRTC_BITRATE_KBPS")) : 20000;

    display = wl_display_connect(NULL);
    if (!display) die("cannot connect to the Wayland display");
    registry = wl_display_get_registry(display);
    wl_registry_add_listener(registry, &registry_listener, NULL);
    wl_display_roundtrip(display);
    wl_display_roundtrip(display);
    if (!screencopy) die("compositor lacks wlr-screencopy");
    if (!dmabuf) die("compositor lacks linux-dmabuf");
    if (!output) die("no output");

    amd_dpy = open_va("/dev/dri/renderD129", &amd_fd);
    intel_dpy = open_va("/dev/dri/renderD128", &intel_fd);
    if (!amd_dpy || !intel_dpy) die("cannot open both VA displays");
    amd_gbm = gbm_create_device(amd_fd);
    intel_gbm = gbm_create_device(intel_fd);
    if (!amd_gbm || !intel_gbm) die("gbm_create_device failed");

    int64_t pts = 0;
    int configured = 0;
    for (;;) {
        frame_ready = frame_failed = 0;
        have_dmabuf_info = 0;
        double t0 = now_ms();
        struct zwlr_screencopy_frame_v1 *frame =
            zwlr_screencopy_manager_v1_capture_output(screencopy, 1, output);
        zwlr_screencopy_frame_v1_add_listener(frame, &frame_listener, NULL);
        while (!have_dmabuf_info && !frame_failed) wl_display_dispatch(display);
        double t_info = now_ms();
        if (frame_failed) {
            zwlr_screencopy_frame_v1_destroy(frame);
            usleep(250000);
            continue;
        }
        int w = (int)dmabuf_w, h = (int)dmabuf_h;
        if (!configured) {
            g_w = w;
            g_h = h;
            setup_nv12(w, h);
            setup_encoder(w, h, qp, gop, kbps);
            configured = 1;
            fprintf(stderr, "zerocopy-capture: %dx%d format=0x%x modifiers=%d\n", w, h, dmabuf_format, nmods);
        }
        // pick a modifier the compositor advertised for this format
        uint64_t modifier = DRM_FORMAT_MOD_LINEAR;
        int have_mod = 0;
        for (int i = 0; i < nmods; i++) {
            if (mods[i] == DRM_FORMAT_MOD_LINEAR) { modifier = mods[i]; have_mod = 1; break; }
        }
        if (!have_mod && nmods > 0) modifier = mods[0];
        struct gbm_bo *bo = gbm_bo_create_with_modifiers(amd_gbm, w, h, dmabuf_format, &modifier, 1);
        if (!bo) {
            zwlr_screencopy_frame_v1_destroy(frame);
            usleep(250000);
            continue;
        }
        int fd = gbm_bo_get_fd(bo);
        unsigned stride = gbm_bo_get_stride(bo);
        struct zwp_linux_buffer_params_v1 *params = zwp_linux_dmabuf_v1_create_params(dmabuf);
        zwp_linux_buffer_params_v1_add(params, fd, 0, 0, stride,
                                       (uint32_t)(modifier >> 32), (uint32_t)(modifier & 0xffffffff));
        struct wl_buffer *buffer = zwp_linux_buffer_params_v1_create_immed(params, w, h, dmabuf_format, 0);
        zwp_linux_buffer_params_v1_destroy(params);
        zwlr_screencopy_frame_v1_copy(frame, buffer);
        while (!frame_ready && !frame_failed) wl_display_dispatch(display);
        double t_ready = now_ms();
        zwlr_screencopy_frame_v1_destroy(frame);
        if (frame_ready) {
            VASurfaceID src = import_argb(fd, stride, modifier, w, h);
            double t_import = now_ms();
            if (src != VA_INVALID_SURFACE) {
                encode_surface(src, w, h, pts++);
                vaDestroySurfaces(amd_dpy, &src, 1);
            }
            double t_enc = now_ms();
            if (getenv("SWAYRTC_TIMING") && pts % 60 == 0) {
                fprintf(stderr, "timing: info %.1f ready %.1f import %.1f encode %.1f total %.1f ms\n",
                        t_info - t0, t_ready - t_info, t_import - t_ready, t_enc - t_import, t_enc - t0);
            }
        }
        wl_buffer_destroy(buffer);
        gbm_bo_destroy(bo);
        close(fd);
    }
    return 0;
}
