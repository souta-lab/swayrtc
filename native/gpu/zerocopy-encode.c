// SwayRTC zero-copy encoder.
//
// Receives ARGB dma-buf file descriptors from the capture process over a unix
// socket (SCM_RIGHTS), converts them to NV12 with the AMD VPP (the compositor
// renders on the AMD GPU, and only AMD exposes a video processor here), then
// encodes on the Intel GPU with libavcodec's h264_vaapi and writes Annex-B to
// stdout.
//
// Why this shape:
//   * Intel iHD has the H.264 encoder but no VPP, AMD has VPP but no encoder.
//   * A linear NV12 buffer is the only layout both drivers can import, so the
//     VPP output goes into one allocated on the Intel device (mmap-able, and
//     importable by AMD).
//   * The encoder wants surfaces from its own pool, so the imported surface is
//     blitted into one with vaPutImage - a GPU copy, no CPU mapping.
#include <libavcodec/avcodec.h>
#include <libavutil/hwcontext.h>
#include <libavutil/hwcontext_vaapi.h>
#include <libavutil/opt.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <gbm.h>
#include <libdrm/drm_fourcc.h>
#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

#ifndef VA_FOURCC_GR88
#define VA_FOURCC_GR88 VA_FOURCC('G', 'R', '8', '8')
#endif

#define MAX_W 3840
#define MAX_H 2160

static int g_w = 1920, g_h = 1080;
static VADisplay amd_dpy, intel_dpy;
static int amd_fd = -1, intel_fd = -1;
static struct gbm_device *intel_gbm;
static struct gbm_bo *nv12_bo;
static VASurfaceID nv12_surf;
static unsigned nv12_stride;
static VAConfigID vpp_cfg;
static VAContextID vpp_ctx;
static VABufferID vpp_buf;
static AVCodecContext *enc;
static AVBufferRef *enc_dev;

static void die(const char *what) {
    fprintf(stderr, "zerocopy-encode: %s\n", what);
    exit(1);
}

static VADisplay open_va(const char *node, int *fd_out) {
    int fd = open(node, O_RDWR);
    if (fd < 0) return NULL;
    VADisplay d = vaGetDisplayDRM(fd);
    int major, minor;
    if (vaInitialize(d, &major, &minor) != VA_STATUS_SUCCESS) return NULL;
    *fd_out = fd;
    return d;
}

static VASurfaceID import_argb(int fd, unsigned stride, unsigned modifier, int w, int h) {
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_ARGB;
    desc.width = w;
    desc.height = h;
    desc.num_objects = 1;
    desc.objects[0].fd = fd;
    desc.objects[0].size = (size_t)stride * h;
    desc.objects[0].drm_format_modifier = modifier;
    desc.num_layers = 1;
    desc.layers[0].drm_format = VA_FOURCC_ARGB;
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
    if (vaCreateSurfaces(amd_dpy, VA_RT_FORMAT_RGB32, w, h, &surf, 1, at, 2) != VA_STATUS_SUCCESS) return VA_INVALID_SURFACE;
    return surf;
}

static void setup_nv12(int w, int h) {
    nv12_bo = gbm_bo_create(intel_gbm, w, h * 3 / 2, GBM_FORMAT_R8, GBM_BO_USE_LINEAR);
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
    if (vaCreateConfig(amd_dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vpp_cfg) != VA_STATUS_SUCCESS)
        die("no VPP config");
    VASurfaceID surfaces[1] = { nv12_surf };
    if (vaCreateContext(amd_dpy, vpp_cfg, w, h, VA_PROGRESSIVE, surfaces, 1, &vpp_ctx) != VA_STATUS_SUCCESS)
        die("no VPP context");
    if (vaCreateBuffer(amd_dpy, vpp_ctx, VAProcPipelineParameterBufferType,
                       sizeof(VAProcPipelineParameterBuffer), 1, NULL, &vpp_buf) != VA_STATUS_SUCCESS)
        die("no VPP buffer");
}

static void setup_encoder(int w, int h, int qp, int gop, int bitrate_kbps) {
    enc_dev = av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_VAAPI);
    if (!enc_dev) die("no hw device");
    AVHWDeviceContext *dc = (AVHWDeviceContext *)enc_dev->data;
    AVVAAPIDeviceContext *vc = (AVVAAPIDeviceContext *)dc->hwctx;
    vc->display = intel_dpy; // share our display: surface ids are per display
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
    enc->bit_rate = (int64_t)bitrate_kbps * 1000;
    av_opt_set(enc->priv_data, "rc_mode", "CQP", 0);
    av_opt_set_int(enc->priv_data, "qp", qp, 0);
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
        av_packet_unref(pkt);
    }
    av_packet_free(&pkt);
}

static void encode_surface(VASurfaceID src, int w, int h, int64_t pts) {
    // VPP: ARGB -> NV12 (AMD)
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
    if (vaBeginPicture(amd_dpy, vpp_ctx, nv12_surf) != VA_STATUS_SUCCESS) return;
    vaRenderPicture(amd_dpy, vpp_ctx, &vpp_buf, 1);
    vaEndPicture(amd_dpy, vpp_ctx);
    vaSyncSurface(amd_dpy, nv12_surf);

    // blit into the encoder's surface (Intel) and encode
    AVFrame *f = av_frame_alloc();
    if (av_hwframe_get_buffer(enc->hw_frames_ctx, f, 0) < 0) {
        av_frame_free(&f);
        return;
    }
    VASurfaceID dst = (VASurfaceID)(uintptr_t)f->data[3];
    VAImage vi;
    if (vaDeriveImage(intel_dpy, nv12_surf, &vi) == VA_STATUS_SUCCESS) {
        vaPutImage(intel_dpy, dst, vi.image_id, 0, 0, w, h, 0, 0, w, h);
        vaDestroyImage(intel_dpy, vi.image_id);
    }
    f->pts = pts;
    if (avcodec_send_frame(enc, f) >= 0) drain();
    av_frame_free(&f);
}

static int recv_fd(int sock, int *w, int *h, unsigned *stride, unsigned *modifier) {
    char buf[64];
    struct iovec iov = { buf, sizeof(buf) };
    char cbuf[CMSG_SPACE(sizeof(int))];
    struct msghdr msg;
    memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cbuf;
    msg.msg_controllen = sizeof(cbuf);
    ssize_t n = recvmsg(sock, &msg, 0);
    if (n <= 0) return -1;
    int fd = -1;
    for (struct cmsghdr *c = CMSG_FIRSTHDR(&msg); c; c = CMSG_NXTHDR(&msg, c)) {
        if (c->cmsg_level == SOL_SOCKET && c->cmsg_type == SCM_RIGHTS) memcpy(&fd, CMSG_DATA(c), sizeof(int));
    }
    if (fd < 0) return -1;
    unsigned sw = 0, sh = 0, sm = 0, ss = 0;
    if (sscanf(buf, "%u %u %u %u", &sw, &sh, &ss, &sm) != 4) { close(fd); return -1; }
    *w = (int)sw;
    *h = (int)sh;
    *stride = ss;
    *modifier = sm;
    return fd;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int sock = 0; // capture process hands fds over stdin
    int qp = getenv("SWAYRTC_QP") ? atoi(getenv("SWAYRTC_QP")) : 24;
    int gop = getenv("SWAYRTC_GOP") ? atoi(getenv("SWAYRTC_GOP")) : 60;
    int kbps = getenv("SWAYRTC_BITRATE_KBPS") ? atoi(getenv("SWAYRTC_BITRATE_KBPS")) : 6000;

    amd_dpy = open_va("/dev/dri/renderD129", &amd_fd);
    intel_dpy = open_va("/dev/dri/renderD128", &intel_fd);
    if (!amd_dpy || !intel_dpy) die("cannot open both VA displays");
    intel_gbm = gbm_create_device(intel_fd);
    if (!intel_gbm) die("gbm_create_device failed");

    int w = 0, h = 0;
    unsigned stride = 0, modifier = 0;
    int first = recv_fd(sock, &w, &h, &stride, &modifier);
    if (first < 0) die("no frame received");
    if (w <= 0 || h <= 0 || w > MAX_W || h > MAX_H) die("bad frame size");
    g_w = w;
    g_h = h;
    setup_nv12(w, h);
    setup_encoder(w, h, qp, gop, kbps);
    fprintf(stderr, "zerocopy-encode: %dx%d stride=%u modifier=0x%x\n", w, h, stride, modifier);

    int fd = first;
    int64_t pts = 0;
    for (;;) {
        VASurfaceID src = import_argb(fd, stride, modifier, w, h);
        if (src != VA_INVALID_SURFACE) {
            encode_surface(src, w, h, pts++);
            vaDestroySurfaces(amd_dpy, &src, 1);
        }
        close(fd);
        fd = recv_fd(sock, &w, &h, &stride, &modifier);
        if (fd < 0) break;
    }
    avcodec_send_frame(enc, NULL);
    drain();
    return 0;
}
