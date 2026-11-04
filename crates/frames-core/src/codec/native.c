/* Opaque, thread-confined codec contexts. Only this file sees FFmpeg ABI structs.
 * Resolve functions lazily: the installer/updater must run without media DLLs. */
#include <libavcodec/avcodec.h>
#include <libavutil/opt.h>
#include <libavutil/hwcontext.h>
#include <libswscale/swscale.h>
#include <libswresample/swresample.h>
#include <stdlib.h>
#include <string.h>
#include <stdio.h>
#ifdef __APPLE__
#include <VideoToolbox/VideoToolbox.h>
#include <dispatch/dispatch.h>
#endif
#ifdef _WIN32
#define COBJMACROS
#include <windows.h>
#include <d3d10_1.h>
#include <libavutil/hwcontext_d3d11va.h>
#define LIB HMODULE
#define SYMBOL GetProcAddress
static LIB open_library(const char *path) {
    wchar_t wide[32768];
    if (!MultiByteToWideChar(CP_UTF8, MB_ERR_INVALID_CHARS, path, -1, wide, 32768)) return NULL;
    return LoadLibraryExW(wide, NULL, LOAD_LIBRARY_SEARCH_DLL_LOAD_DIR | LOAD_LIBRARY_SEARCH_SYSTEM32);
}
#else
#include <dlfcn.h>

#define LIB void *
#define SYMBOL dlsym
static LIB open_library(const char *path) { return dlopen(path, RTLD_NOW | RTLD_LOCAL); }
#endif
/* Periodic keyframe interval for every backend (VideoToolbox, NVENC, QSV,
 * AMF, MF, libaom, libvpx), in frames. Keyframes are also sent on demand: at
 * stream start, on a client recovery request, after an encoder restart, and
 * on a meaningful bitrate increase. The periodic one stays as the safety net
 * until keyframe-free recovery (NVENC reference invalidation, VideoToolbox
 * long-term references) lands: a netsim A/B on 2026-11-04 with on-demand-only
 * keyframes saved ~35% bitrate on a clean LAN (VideoToolbox) but lowered
 * delivered frames on lossy paths (VideoToolbox 3.2 -> 0.3 fps, NVENC
 * 7.3 -> 4.5 fps). Keep any value <= 65535 (QSV stores it in 16 bits). */
#define FRAMES_KEYFRAME_INTERVAL 60

/* Rate shape for encoders opened on this thread: GOP length and VBV size as
 * a divisor of the bitrate (30 = 1/30 s). v1 never changes it. The v2 engine
 * (frames-codec) sets an infinite GOP, since it recovers from loss without
 * keyframes, and a one-frame VBV (divisor = fps). */
#if defined(_MSC_VER)
#define FRAMES_THREAD_LOCAL __declspec(thread)
#else
#define FRAMES_THREAD_LOCAL __thread
#endif
static FRAMES_THREAD_LOCAL int frames_shape_gop = FRAMES_KEYFRAME_INTERVAL;
static FRAMES_THREAD_LOCAL int frames_shape_vbv_divisor = 30;
/* gop <= 0 means keyframes only on request. */
void frames_encoder_shape(int gop, int vbv_divisor) {
    frames_shape_gop = gop;
    frames_shape_vbv_divisor = vbv_divisor > 0 ? vbv_divisor : 30;
}
static int shape_gop(const char *name, int gop) {
    if (gop > 0) return gop;
    /* QSV stores the GOP in 16 bits; elsewhere a value no session reaches. */
    return strstr(name, "_qsv") ? 65535 : 1000000;
}

#define API(F) \
 F(unsigned, swresample_version, (void)) \
 F(int, swr_alloc_set_opts2, (struct SwrContext **, const AVChannelLayout *, enum AVSampleFormat, int, const AVChannelLayout *, enum AVSampleFormat, int, int, void *)) \
 F(int, swr_init, (struct SwrContext *)) \
 F(int, swr_convert, (struct SwrContext *, uint8_t **, int, const uint8_t **, int)) \
 F(int, swr_get_out_samples, (struct SwrContext *, int)) \
 F(void, swr_free, (struct SwrContext **)) \
 F(void, av_channel_layout_default, (AVChannelLayout *, int)) \
 F(void, av_channel_layout_uninit, (AVChannelLayout *)) \
 F(unsigned, avcodec_version, (void)) \
 F(unsigned, avutil_version, (void)) \
 F(unsigned, swscale_version, (void)) \
 F(const char *, avcodec_configuration, (void)) \
 F(const AVCodec *, avcodec_find_encoder_by_name, (const char *)) \
 F(const AVCodec *, avcodec_find_decoder_by_name, (const char *)) \
 F(AVCodecContext *, avcodec_alloc_context3, (const AVCodec *)) \
 F(void, avcodec_free_context, (AVCodecContext **)) \
 F(int, avcodec_open2, (AVCodecContext *, const AVCodec *, AVDictionary **)) \
 F(int, avcodec_send_frame, (AVCodecContext *, const AVFrame *)) \
 F(int, avcodec_receive_packet, (AVCodecContext *, AVPacket *)) \
 F(void, avcodec_flush_buffers, (AVCodecContext *)) \
 F(int, avcodec_send_packet, (AVCodecContext *, const AVPacket *)) \
 F(int, avcodec_receive_frame, (AVCodecContext *, AVFrame *)) \
 F(const AVCodecHWConfig *, avcodec_get_hw_config, (const AVCodec *, int)) \
 F(AVPacket *, av_packet_alloc, (void)) \
 F(void, av_packet_free, (AVPacket **)) \
 F(void, av_packet_unref, (AVPacket *)) \
 F(int, av_new_packet, (AVPacket *, int)) \
 F(AVFrame *, av_frame_alloc, (void)) \
 F(void, av_frame_free, (AVFrame **)) \
 F(AVFrame *, av_frame_clone, (const AVFrame *)) \
 F(void, av_frame_unref, (AVFrame *)) \
 F(int, av_frame_get_buffer, (AVFrame *, int)) \
 F(int, av_frame_make_writable, (AVFrame *)) \
 F(int, av_opt_set, (void *, const char *, const char *, int)) \
 F(int, av_opt_set_int, (void *, const char *, int64_t, int)) \
 F(int, av_strerror, (int, char *, size_t)) \
 F(void, av_log_set_level, (int)) \
 F(int, av_hwdevice_ctx_create, (AVBufferRef **, enum AVHWDeviceType, const char *, AVDictionary *, int)) \
 F(AVBufferRef *, av_hwdevice_ctx_alloc, (enum AVHWDeviceType)) \
 F(int, av_hwdevice_ctx_init, (AVBufferRef *)) \
 F(AVBufferRef *, av_hwframe_ctx_alloc, (AVBufferRef *)) \
 F(int, av_hwframe_ctx_init, (AVBufferRef *)) \
 F(int, av_hwframe_get_buffer, (AVBufferRef *, AVFrame *, int)) \
 F(AVBufferRef *, av_buffer_ref, (const AVBufferRef *)) \
 F(void, av_buffer_unref, (AVBufferRef **)) \
 F(int, av_hwframe_transfer_data, (AVFrame *, const AVFrame *, int)) \
 F(struct SwsContext *, sws_getCachedContext, (struct SwsContext *, int, int, enum AVPixelFormat, int, int, enum AVPixelFormat, int, SwsFilter *, SwsFilter *, const double *)) \
 F(int, sws_scale, (struct SwsContext *, const uint8_t *const [], const int [], int, int, uint8_t *const [], const int [])) \
 F(void, sws_freeContext, (struct SwsContext *))
#define DECLARE(ret, name, args) static ret (*p_##name) args;
API(DECLARE)

int frames_media_init(const char *util, const char *scale, const char *resample, const char *codec, char *error, int capacity) {
    LIB libs[4] = {open_library(util), open_library(scale), open_library(resample), open_library(codec)};
    if (!libs[0] || !libs[1] || !libs[2] || !libs[3]) { snprintf(error, capacity, "Cannot load bundled codec libraries"); return -1; }
#define LOAD(ret, name, args) \
    for (int i = 0; i < 4 && !p_##name; i++) p_##name = (ret (*) args)(void *) SYMBOL(libs[i], #name); \
    if (!p_##name) { snprintf(error, capacity, "Missing codec function %s", #name); return -1; }
    API(LOAD)
    if ((p_avcodec_version() >> 16) != LIBAVCODEC_VERSION_MAJOR ||
        (p_avutil_version() >> 16) != LIBAVUTIL_VERSION_MAJOR ||
        (p_swscale_version() >> 16) != LIBSWSCALE_VERSION_MAJOR ||
        (p_swresample_version() >> 16) != LIBSWRESAMPLE_VERSION_MAJOR) {
        snprintf(error, capacity, "Codec runtime/header ABI mismatch"); return -1;
    }
    /* Our release artifacts use LGPL shared libraries exclusively. */
    if (strstr(p_avcodec_configuration(), "--enable-gpl")) {
        snprintf(error, capacity, "Frames requires the prepared LGPL media runtime"); return -1;
    }
    p_av_log_set_level(AV_LOG_ERROR);
    return 0;
}

void *frames_resampler_open(int in_rate, int in_channels, int out_rate, int out_channels) {
    struct SwrContext *context=NULL;
    AVChannelLayout in_layout, out_layout;
    p_av_channel_layout_default(&in_layout,in_channels);
    p_av_channel_layout_default(&out_layout,out_channels);
    int result=p_swr_alloc_set_opts2(&context,&out_layout,AV_SAMPLE_FMT_FLT,out_rate,&in_layout,AV_SAMPLE_FMT_FLT,in_rate,0,NULL);
    p_av_channel_layout_uninit(&in_layout);p_av_channel_layout_uninit(&out_layout);
    if (result<0 || p_swr_init(context)<0) {p_swr_free(&context);return NULL;}
    return context;
}
void frames_resampler_close(void *context) {struct SwrContext *c=context;p_swr_free(&c);}
int frames_resampler_capacity(void *context,int frames) {return p_swr_get_out_samples(context,frames);}
int frames_resampler_convert(void *context,const float *input,int frames,float *output,int capacity) {
    const uint8_t *in=(const uint8_t *)input;uint8_t *out=(uint8_t *)output;
    return p_swr_convert(context,&out,capacity,&in,frames);
}

void frames_gpu_converter_close(void *opaque);
typedef struct Media {
    AVCodecContext *ctx;
    AVFrame *frame, *cpu, *converted;
    AVPacket *packet;
    struct SwsContext *sws;
    enum AVPixelFormat hw_format;
    int width, height, padded_width, padded_height;
    int64_t pts;
    int force_keyframe;
    uint8_t *padded;
    struct SwsContext *scale;
    uint8_t *scaled;
    int bitrate;
    int gop, vbv_divisor;
    void *gpu_converter;
    int gpu_encoder;
#ifdef __APPLE__
    VTCompressionSessionRef vt;
    CVPixelBufferPoolRef vt_pool;
    dispatch_semaphore_t ready;
    uint8_t *vt_bytes;
    int vt_size, vt_key, vt_error, vt_received, fps;
    CMTime vt_pts;
#endif
} Media;

void frames_media_close(Media *m) {
    if (!m) return;
#ifdef __APPLE__
    if (m->vt) { VTCompressionSessionInvalidate(m->vt); CFRelease(m->vt); }
    if (m->ready) dispatch_release(m->ready);
    if (m->vt_pool) CFRelease(m->vt_pool);
    free(m->vt_bytes);
#endif
    frames_gpu_converter_close(m->gpu_converter);
    p_avcodec_free_context(&m->ctx);
    p_av_frame_free(&m->frame);
    p_av_frame_free(&m->cpu);
    p_av_frame_free(&m->converted);
    p_av_packet_free(&m->packet);
    p_sws_freeContext(m->sws);
    free(m->padded); free(m->scaled); p_sws_freeContext(m->scale);
    free(m);
}
static int failure(int code, char *error, int capacity) {
    if (code < 0) p_av_strerror(code, error, capacity);
    return code;
}
static Media *create(const AVCodec *codec, int width, int height) {
    if (!codec || width < 1 || height < 1 || width > 8192 || height > 8192) return NULL;
    Media *m = calloc(1, sizeof(*m));
    if (!m) return NULL;
    m->ctx = p_avcodec_alloc_context3(codec);
    m->frame = p_av_frame_alloc(); m->cpu = p_av_frame_alloc();
    m->converted = p_av_frame_alloc(); m->packet = p_av_packet_alloc();
    m->width = width; m->height = height; m->hw_format = AV_PIX_FMT_NONE;
    m->gop = shape_gop(codec->name, frames_shape_gop); m->vbv_divisor = frames_shape_vbv_divisor;
    if (!m->ctx || !m->frame || !m->cpu || !m->converted || !m->packet) { frames_media_close(m); return NULL; }
    return m;
}
static void option(Media *m, const char *name, const char *value) {
    p_av_opt_set(m->ctx->priv_data, name, value, 0);
}

#ifdef __APPLE__
static void vt_append(Media *m,const uint8_t *data,size_t size) {
    if (size>8*1024*1024 || (size_t)m->vt_size+size+4>8*1024*1024) {m->vt_error=-1;return;}
    uint8_t *bytes=realloc(m->vt_bytes,(size_t)m->vt_size+size+4);
    if (!bytes) {m->vt_error=-1;return;}
    m->vt_bytes=bytes;
    memcpy(bytes+m->vt_size,"\0\0\0\1",4);
    memcpy(bytes+m->vt_size+4,data,size);m->vt_size+=(int)size+4;
}
static void vt_output(void *opaque,void *frame,OSStatus status,VTEncodeInfoFlags flags,CMSampleBufferRef sample) {
    (void)frame;(void)flags;Media *m=opaque;
    m->vt_error=status;
    if (!status && sample && CMSampleBufferDataIsReady(sample)) {
        CFArrayRef attachments=CMSampleBufferGetSampleAttachmentsArray(sample,false);
        CFDictionaryRef attachment=attachments && CFArrayGetCount(attachments)>0 ? CFArrayGetValueAtIndex(attachments,0):NULL;
        m->vt_key=!attachment || !CFDictionaryContainsKey(attachment,kCMSampleAttachmentKey_NotSync);
        CMFormatDescriptionRef format=CMSampleBufferGetFormatDescription(sample);
        int hevc=CMFormatDescriptionGetMediaSubType(format)==kCMVideoCodecType_HEVC;
        if (m->vt_key) {
            size_t count=0,size=0;int length=0;const uint8_t *parameter=NULL;
            OSStatus result=hevc?CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,0,&parameter,&size,&count,&length):
                CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,0,&parameter,&size,&count,&length);
            if (!result) for(size_t i=0;i<count;i++) {
                result=hevc?CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(format,i,&parameter,&size,NULL,NULL):
                    CMVideoFormatDescriptionGetH264ParameterSetAtIndex(format,i,&parameter,&size,NULL,NULL);
                if (!result) vt_append(m,parameter,size); else m->vt_error=result;
            }
        }
        CMBlockBufferRef block=CMSampleBufferGetDataBuffer(sample);
        size_t size=CMBlockBufferGetDataLength(block),offset=0;
        while (offset+4<=size) {
            uint8_t length[4];
            if (CMBlockBufferCopyDataBytes(block,offset,4,length)) {m->vt_error=-1;break;}
            size_t count=((size_t)length[0]<<24)|((size_t)length[1]<<16)|((size_t)length[2]<<8)|length[3];
            offset+=4;
            if (!count || count>size-offset) {m->vt_error=-1;break;}
            uint8_t *nal=malloc(count);
            if (!nal) {m->vt_error=-1;break;}
            if (CMBlockBufferCopyDataBytes(block,offset,count,nal)) m->vt_error=-1;
            else vt_append(m,nal,count);
            free(nal);offset+=count;
        }
    }
    /* A successful callback without a sample is a rate-control picture drop,
       not a failed hardware backend. The next capture/recovery request retries. */
    dispatch_semaphore_signal(m->ready);
}
static int vt_open(Media *m,const char *name,int fps) {
    int hevc=strstr(name,"hevc")!=NULL;m->fps=fps;m->vt_pts=kCMTimeZero;
    const void *keys[]={kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,kVTVideoEncoderSpecification_EnableLowLatencyRateControl};
    const void *values[]={kCFBooleanTrue,kCFBooleanTrue};
    CFDictionaryRef spec=CFDictionaryCreate(NULL,keys,values,2,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    OSStatus result=VTCompressionSessionCreate(NULL,m->padded_width,m->padded_height,
        hevc?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,spec,NULL,NULL,vt_output,m,&m->vt);
    CFRelease(spec);if(result)return result;
    m->ready=dispatch_semaphore_create(0);if(!m->ready)return -1;
    if (VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_RealTime,kCFBooleanTrue) ||
        VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_AllowFrameReordering,kCFBooleanFalse)) return -1;
    int bitrate=m->bitrate?m->bitrate:12000000,gop=m->gop;
    CFNumberRef number=CFNumberCreate(NULL,kCFNumberIntType,&bitrate);
    result=VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_AverageBitRate,number);CFRelease(number);if(result)return result;
    number=CFNumberCreate(NULL,kCFNumberIntType,&fps);
    VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_ExpectedFrameRate,number);CFRelease(number);
    number=CFNumberCreate(NULL,kCFNumberIntType,&gop);
    VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_MaxKeyFrameInterval,number);CFRelease(number);
    if(!hevc)VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_ProfileLevel,kVTProfileLevel_H264_ConstrainedHigh_AutoLevel);
    int pixel_format=kCVPixelFormatType_32BGRA;
    CFNumberRef w=CFNumberCreate(NULL,kCFNumberIntType,&m->padded_width),h=CFNumberCreate(NULL,kCFNumberIntType,&m->padded_height),pixel=CFNumberCreate(NULL,kCFNumberIntType,&pixel_format);
    const void *pool_keys[]={kCVPixelBufferWidthKey,kCVPixelBufferHeightKey,kCVPixelBufferPixelFormatTypeKey};
    const void *pool_values[]={w,h,pixel};
    CFDictionaryRef attributes=CFDictionaryCreate(NULL,pool_keys,pool_values,3,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    result=CVPixelBufferPoolCreate(NULL,NULL,attributes,&m->vt_pool);CFRelease(attributes);CFRelease(w);CFRelease(h);CFRelease(pixel);
    return result ? result:VTCompressionSessionPrepareToEncodeFrames(m->vt);
}
static int vt_send(Media *m,const uint8_t *bgra) {
    CVPixelBufferRef buffer=NULL;
    OSStatus result=CVPixelBufferPoolCreatePixelBuffer(NULL,m->vt_pool,&buffer);
    if(result)return result;
    CVPixelBufferLockBaseAddress(buffer,0);
    uint8_t *dest=CVPixelBufferGetBaseAddress(buffer);size_t stride=CVPixelBufferGetBytesPerRow(buffer);
    for(int y=0;y<m->padded_height;y++) {
        const uint8_t *row=bgra+(size_t)(y<m->height?y:m->height-1)*m->width*4;
        memcpy(dest+y*stride,row,(size_t)m->width*4);
        for(int x=m->width;x<m->padded_width;x++)memcpy(dest+y*stride+x*4,row+(m->width-1)*4,4);
    }
    CVPixelBufferUnlockBaseAddress(buffer,0);
    free(m->vt_bytes);m->vt_bytes=NULL;m->vt_size=0;m->vt_error=0;m->vt_received=0;
    CFDictionaryRef properties=NULL;
    if(m->force_keyframe) {
        const void *key=kVTEncodeFrameOptionKey_ForceKeyFrame,*value=kCFBooleanTrue;
        properties=CFDictionaryCreate(NULL,&key,&value,1,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    }
    m->force_keyframe=0;
    CMTime duration=CMTimeMake(1,m->fps),presentation=m->vt_pts;
    m->vt_pts=CMTimeAdd(m->vt_pts,duration);
    result=VTCompressionSessionEncodeFrame(m->vt,buffer,presentation,duration,properties,NULL,NULL);
    if(properties)CFRelease(properties);
    CVPixelBufferRelease(buffer);
    if(result)return result;
    if(dispatch_semaphore_wait(m->ready,dispatch_time(DISPATCH_TIME_NOW,2*NSEC_PER_SEC)))return -1;
    return m->vt_error;
}
#endif

Media *frames_encoder_open(const char *name, int width, int height, int fps, int bitrate, char *error, int capacity) {
    const AVCodec *codec = p_avcodec_find_encoder_by_name(name);
    Media *m = create(codec, width, height);
    if (!m) { snprintf(error, capacity, "Encoder %s unavailable", name); return NULL; }
    AVCodecContext *c = m->ctx; m->bitrate=bitrate;
    m->padded_width = width < 256 ? 256 : (width + 1) & ~1;
    m->padded_height = height < 256 ? 256 : (height + 1) & ~1;
#ifdef __APPLE__
    if (strstr(name,"_videotoolbox")) {
        if (vt_open(m,name,fps)) {snprintf(error,capacity,"Cannot initialize low-latency VideoToolbox");frames_media_close(m);return NULL;}
        return m;
    }
#endif
    c->width = m->padded_width; c->height = m->padded_height;
    c->time_base = (AVRational){1, fps}; c->framerate = (AVRational){fps, 1};
    c->bit_rate = bitrate; c->rc_max_rate = bitrate; c->rc_buffer_size = bitrate/m->vbv_divisor; c->gop_size = m->gop; c->max_b_frames = 0;
    c->flags |= AV_CODEC_FLAG_LOW_DELAY;
    c->thread_count = 2; c->thread_type = FF_THREAD_SLICE;
    c->pix_fmt = AV_PIX_FMT_YUV420P;
    if (strstr(name, "_amf") || strstr(name, "_qsv")) c->pix_fmt = AV_PIX_FMT_NV12;
    if (strstr(name, "_nvenc")) {
        option(m,"rc","cbr"); option(m,"preset","p1"); option(m,"tune","ull"); option(m,"zerolatency","1");
        option(m,"rc-lookahead","0"); option(m,"delay","0"); option(m,"forced-idr","1");
    } else if (strstr(name,"_amf")) {
        option(m,"usage","ultralowlatency"); option(m,"quality","speed");
        option(m,"preanalysis","0"); option(m,"async_depth","1");
    } else if (strstr(name,"_qsv")) {
        option(m,"preset","veryfast"); option(m,"async_depth","1"); option(m,"look_ahead","0"); option(m,"forced_idr","1");
    } else if (strstr(name,"_videotoolbox")) {
        option(m,"realtime","1"); option(m,"allow_sw","0");
    } else if (strstr(name,"libvpx") || strstr(name,"libaom")) {
        option(m,"usage","realtime"); option(m,"deadline","realtime");
        option(m,"cpu-used","8"); option(m,"lag-in-frames","0"); option(m,"row-mt","1");
    }
    int result = p_avcodec_open2(c, codec, NULL);
    if (result < 0) { failure(result,error,capacity); frames_media_close(m); return NULL; }
    m->converted->format = c->pix_fmt;
    m->converted->width = c->width; m->converted->height = c->height;
    result = p_av_frame_get_buffer(m->converted, 32);
    m->padded = malloc((size_t)c->width * c->height * 4);
    if (result < 0 || !m->padded) { snprintf(error,capacity,"Cannot allocate encoder frame"); frames_media_close(m); return NULL; }
    return m;
}

void frames_encoder_request_keyframe(Media *m) { m->force_keyframe=1; }

/* Send exactly one captured picture. The caller drains complete AVPackets. */
int frames_encoder_send(Media *m, const uint8_t *bgra, char *error, int capacity) {
#ifdef __APPLE__
    if(m->vt) {int result=vt_send(m,bgra);if(result)snprintf(error,capacity,"VideoToolbox encode failed (%d)",result);return result ? -1:0;}
#endif
    int stride = m->padded_width * 4;
    for (int y=0; y<m->padded_height; y++) {
        const uint8_t *row = bgra + (size_t)(y < m->height ? y : m->height-1) * m->width * 4;
        uint8_t *dest = m->padded + (size_t)y * stride;
        memcpy(dest, row, (size_t)m->width*4);
        for (int x=m->width; x<m->padded_width; x++) memcpy(dest+x*4, row+(m->width-1)*4, 4);
    }
    int result = p_av_frame_make_writable(m->converted);
    if (result < 0) return failure(result,error,capacity);
    m->sws = p_sws_getCachedContext(m->sws, m->padded_width,m->padded_height,AV_PIX_FMT_BGRA,
        m->padded_width,m->padded_height,m->ctx->pix_fmt,SWS_FAST_BILINEAR,NULL,NULL,NULL);
    if (!m->sws) return failure(AVERROR(ENOMEM),error,capacity);
    const uint8_t *source[4] = {m->padded,NULL,NULL,NULL}; int lines[4] = {stride,0,0,0};
    result = p_sws_scale(m->sws,source,lines,0,m->padded_height,m->converted->data,m->converted->linesize);
    if (result < 0) return failure(result,error,capacity);
    m->converted->pts = m->pts++;
    m->converted->pict_type=m->force_keyframe ? AV_PICTURE_TYPE_I:AV_PICTURE_TYPE_NONE;
    m->force_keyframe=0;
    return failure(p_avcodec_send_frame(m->ctx,m->converted),error,capacity);
}
/* 0: applied, 1: this encoder needs a controlled restart. */
int frames_encoder_set_rate(Media *m, int bitrate, int fps) {
    if (bitrate < 128000 || fps < 1 || fps > 240) return -1;
#ifdef __APPLE__
    if (m->vt) {
        CFNumberRef n=CFNumberCreate(NULL,kCFNumberIntType,&bitrate);
        OSStatus result=VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_AverageBitRate,n);CFRelease(n);
        if(result)return 1;
        n=CFNumberCreate(NULL,kCFNumberIntType,&fps);
        VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_ExpectedFrameRate,n);CFRelease(n);
        m->fps=fps;m->bitrate=bitrate;return 0;
    }
#endif
    m->ctx->bit_rate=bitrate; m->ctx->rc_max_rate=bitrate; m->ctx->rc_buffer_size=bitrate/m->vbv_divisor;
    m->ctx->framerate=(AVRational){fps,1};
    m->bitrate=bitrate;
    /* libaom does not consume live AVCodecContext rate changes. Media Foundation
       similarly needs a new encoder. Do not claim an update was applied. */
    return strstr(m->ctx->codec->name,"_mf") || strstr(m->ctx->codec->name,"libaom") ? 1 : 0;
}
int frames_encoder_send_scaled(Media *m,const uint8_t *bgra,int width,int height,char *error,int capacity) {
    if(width<1||height<1||width>8192||height>8192)return -1;
    if(width==m->width && height==m->height)return frames_encoder_send(m,bgra,error,capacity);
    if(!m->scaled)m->scaled=malloc((size_t)m->width*m->height*4);
    if(!m->scaled)return failure(AVERROR(ENOMEM),error,capacity);
    m->scale=p_sws_getCachedContext(m->scale,width,height,AV_PIX_FMT_BGRA,m->width,m->height,AV_PIX_FMT_BGRA,SWS_FAST_BILINEAR,NULL,NULL,NULL);
    if(!m->scale)return failure(AVERROR(ENOMEM),error,capacity);
    const uint8_t *src[4]={bgra,NULL,NULL,NULL};int lines[4]={width*4,0,0,0};
    uint8_t *dst[4]={m->scaled,NULL,NULL,NULL};int stride[4]={m->width*4,0,0,0};
    int result=p_sws_scale(m->scale,src,lines,0,height,dst,stride);
    return result<0 ? failure(result,error,capacity):frames_encoder_send(m,m->scaled,error,capacity);
}
int frames_encoder_receive(Media *m, const uint8_t **data, int *size, int *key, char *error, int capacity) {
#ifdef __APPLE__
    if(m->vt) {if(m->vt_received || !m->vt_size)return 0;m->vt_received=1;*data=m->vt_bytes;*size=m->vt_size;*key=m->vt_key;return 1;}
#endif
    p_av_packet_unref(m->packet);
    int result = p_avcodec_receive_packet(m->ctx,m->packet);
    if (result == AVERROR(EAGAIN) || result == AVERROR_EOF) return 0;
    if (result < 0) return failure(result,error,capacity);
    *data = m->packet->data; *size = m->packet->size; *key = !!(m->packet->flags & AV_PKT_FLAG_KEY);
    return 1;
}

static enum AVPixelFormat select_format(AVCodecContext *c, const enum AVPixelFormat *formats) {
    Media *m = c->opaque;
    for (const enum AVPixelFormat *f=formats; *f!=AV_PIX_FMT_NONE; f++) if (*f==m->hw_format) return *f;
    /* A failed hardware negotiation must be retried in a fresh software context. */
    return AV_PIX_FMT_NONE;
}
Media *frames_decoder_open(const char *name, int width, int height, int hardware, char *error, int capacity) {
    const AVCodec *codec = p_avcodec_find_decoder_by_name(name);
    Media *m = create(codec,width,height);
    if (!m) { snprintf(error,capacity,"Decoder %s unavailable",name); return NULL; }
    AVCodecContext *c = m->ctx;
    c->width=width; c->height=height; c->flags |= AV_CODEC_FLAG_LOW_DELAY;
    c->thread_count=2; c->thread_type=FF_THREAD_SLICE;
    c->max_pixels = 8192LL*8192;
    if (!strcmp(name,"libdav1d")) { option(m,"max_frame_delay","1"); option(m,"framethreads","1"); }
    if (hardware) {
#ifdef _WIN32
        enum AVHWDeviceType type = AV_HWDEVICE_TYPE_D3D11VA;
#else
        enum AVHWDeviceType type = AV_HWDEVICE_TYPE_VIDEOTOOLBOX;
#endif
        for (int i=0;;i++) {
            const AVCodecHWConfig *cfg = p_avcodec_get_hw_config(codec,i);
            if (!cfg) break;
            if (cfg->device_type==type && (cfg->methods&AV_CODEC_HW_CONFIG_METHOD_HW_DEVICE_CTX)) {
                m->hw_format=cfg->pix_fmt; break;
            }
        }
        if (m->hw_format==AV_PIX_FMT_NONE || p_av_hwdevice_ctx_create(&c->hw_device_ctx,type,NULL,NULL,0)<0) {
            snprintf(error,capacity,"Hardware decode unavailable"); frames_media_close(m); return NULL;
        }
        c->opaque=m; c->get_format=select_format;
    }
    int result=p_avcodec_open2(c,codec,NULL);
    if (result<0) { failure(result,error,capacity); frames_media_close(m); return NULL; }
    return m;
}
int frames_decoder_send(Media *m, const uint8_t *data, int size, char *error, int capacity) {
    p_av_packet_unref(m->packet);
    int result=p_av_new_packet(m->packet,size);
    if (result<0) return failure(result,error,capacity);
    /* av_new_packet supplies the zero padding required by optimized decoders. */
    memcpy(m->packet->data,data,size);
    return failure(p_avcodec_send_packet(m->ctx,m->packet),error,capacity);
}
int frames_decoder_receive(Media *m, uint8_t *bgra, char *error, int capacity) {
    p_av_frame_unref(m->frame); p_av_frame_unref(m->cpu);
    int result=p_avcodec_receive_frame(m->ctx,m->frame);
    if (result==AVERROR(EAGAIN) || result==AVERROR_EOF) return 0;
    if (result<0) return failure(result,error,capacity);
    AVFrame *f=m->frame;
    if (f->format==m->hw_format) {
        result=p_av_hwframe_transfer_data(m->cpu,f,0);
        if (result<0) return failure(result,error,capacity);
        f=m->cpu;
    }
    if (f->width<m->width || f->height<m->height || f->width>8192 || f->height>8192) {
        snprintf(error,capacity,"Invalid decoded picture dimensions"); return -1;
    }
    /* Crop encoder padding by converting the top-left visible rectangle. */
    m->sws=p_sws_getCachedContext(m->sws,m->width,m->height,(enum AVPixelFormat)f->format,
        m->width,m->height,AV_PIX_FMT_BGRA,SWS_FAST_BILINEAR,NULL,NULL,NULL);
    if (!m->sws) return failure(AVERROR(ENOMEM),error,capacity);
    uint8_t *dest[4]={bgra,NULL,NULL,NULL}; int lines[4]={m->width*4,0,0,0};
    result=p_sws_scale(m->sws,(const uint8_t *const *)f->data,f->linesize,0,m->height,dest,lines);
    return result<0 ? failure(result,error,capacity) : 1;
}

/* The AVFrame reference pins the decoder pool surface until presentation releases it. */
void frames_surface_release(void *surface) { AVFrame *f=surface;p_av_frame_free(&f); }
int frames_decoder_receive_surface(Media *m,void **lease,void **texture,int *subresource,int *matrix,int *full,char *error,int capacity) {
#ifdef _WIN32
    p_av_frame_unref(m->frame);
    int result=p_avcodec_receive_frame(m->ctx,m->frame);
    if(result==AVERROR(EAGAIN)||result==AVERROR_EOF)return 0;
    if(result<0)return failure(result,error,capacity);
    AVFrame *f=m->frame;
    if(f->format!=AV_PIX_FMT_D3D11 || f->width<m->width || f->height<m->height || f->width>8192 || f->height>8192) {
        snprintf(error,capacity,"Hardware surface format/dimensions changed");return -1;
    }
    AVFrame *ref=p_av_frame_clone(f);if(!ref)return failure(AVERROR(ENOMEM),error,capacity);
    *lease=ref;*texture=f->data[0];*subresource=(int)(intptr_t)f->data[1];
    *matrix=f->colorspace==AVCOL_SPC_BT709;*full=f->color_range==AVCOL_RANGE_JPEG;return 1;
#else
    (void)m;(void)lease;(void)texture;(void)subresource;(void)matrix;(void)full;(void)error;(void)capacity;return -1;
#endif
}

#ifdef _WIN32
typedef struct GpuConverter {
    ID3D11Device *device;
    ID3D11DeviceContext *context;
    ID3D11VideoDevice *video;
    ID3D11VideoContext *video_context;
    ID3D11VideoProcessorEnumerator *enumerator;
    ID3D11VideoProcessor *processor;
    ID3D11Texture2D *readback, *rgb;
    int width,height;
} GpuConverter;
void frames_gpu_converter_close(void *opaque) {
    GpuConverter *g=opaque;if(!g)return;
    if(g->readback)ID3D11Texture2D_Release(g->readback);
    if(g->rgb)ID3D11Texture2D_Release(g->rgb);
    if(g->processor)ID3D11VideoProcessor_Release(g->processor);
    if(g->enumerator)ID3D11VideoProcessorEnumerator_Release(g->enumerator);
    if(g->video_context)ID3D11VideoContext_Release(g->video_context);
    if(g->video)ID3D11VideoDevice_Release(g->video);
    if(g->context)ID3D11DeviceContext_Release(g->context);
    if(g->device)ID3D11Device_Release(g->device);
    free(g);
}
void *frames_gpu_converter_open(void *source,int width,int height) {
    ID3D11Texture2D *texture=source;GpuConverter *g=calloc(1,sizeof(*g));if(!g)return NULL;
    D3D11_TEXTURE2D_DESC desc;ID3D11Texture2D_GetDesc(texture,&desc);
    ID3D11Texture2D_GetDevice(texture,&g->device);ID3D11Device_GetImmediateContext(g->device,&g->context);
    ID3D10Multithread *multi=NULL;
    if(SUCCEEDED(ID3D11DeviceContext_QueryInterface(g->context,&IID_ID3D10Multithread,(void **)&multi))) {
        ID3D10Multithread_SetMultithreadProtected(multi,TRUE);ID3D10Multithread_Release(multi);
    }
    if(FAILED(ID3D11Device_QueryInterface(g->device,&IID_ID3D11VideoDevice,(void **)&g->video)) ||
       FAILED(ID3D11DeviceContext_QueryInterface(g->context,&IID_ID3D11VideoContext,(void **)&g->video_context)))goto fail;
    D3D11_VIDEO_PROCESSOR_CONTENT_DESC content={0};content.InputFrameFormat=D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE;
    content.InputWidth=desc.Width;content.InputHeight=desc.Height;content.OutputWidth=width;content.OutputHeight=height;
    content.InputFrameRate.Numerator=content.OutputFrameRate.Numerator=60;
    content.InputFrameRate.Denominator=content.OutputFrameRate.Denominator=1;content.Usage=D3D11_VIDEO_USAGE_PLAYBACK_NORMAL;
    if(FAILED(ID3D11VideoDevice_CreateVideoProcessorEnumerator(g->video,&content,&g->enumerator)) ||
       FAILED(ID3D11VideoDevice_CreateVideoProcessor(g->video,g->enumerator,0,&g->processor)))goto fail;
    g->width=width;g->height=height;return g;
fail: frames_gpu_converter_close(g);return NULL;
}
int frames_gpu_convert(void *opaque,void *source,int input_slice,void *target,int output_slice,int visible_width,int visible_height,int matrix,int full) {
    GpuConverter *g=opaque;if(!g)return -1;
    ID3D11VideoProcessorInputView *input=NULL;ID3D11VideoProcessorOutputView *output=NULL;
    D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC in={0};in.ViewDimension=D3D11_VPIV_DIMENSION_TEXTURE2D;in.Texture2D.ArraySlice=input_slice;
    HRESULT result=ID3D11VideoDevice_CreateVideoProcessorInputView(g->video,(ID3D11Resource *)source,g->enumerator,&in,&input);if(FAILED(result))return -1;
    D3D11_TEXTURE2D_DESC desc;ID3D11Texture2D_GetDesc((ID3D11Texture2D *)target,&desc);
    D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC out={0};
    if(desc.ArraySize>1){out.ViewDimension=D3D11_VPOV_DIMENSION_TEXTURE2DARRAY;out.Texture2DArray.FirstArraySlice=output_slice;out.Texture2DArray.ArraySize=1;}
    else out.ViewDimension=D3D11_VPOV_DIMENSION_TEXTURE2D;
    result=ID3D11VideoDevice_CreateVideoProcessorOutputView(g->video,(ID3D11Resource *)target,g->enumerator,&out,&output);if(FAILED(result)){ID3D11VideoProcessorInputView_Release(input);return -1;}
    RECT src={0,0,visible_width,visible_height},dst={0,0,g->width,g->height};
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE color={0};color.YCbCr_Matrix=matrix!=0;color.Nominal_Range=full?2:1;
    D3D11_VIDEO_PROCESSOR_COLOR_SPACE rgb={0};rgb.Nominal_Range=2;
    D3D11_TEXTURE2D_DESC source_desc;ID3D11Texture2D_GetDesc((ID3D11Texture2D *)source,&source_desc);
    int source_rgb=source_desc.Format==DXGI_FORMAT_B8G8R8A8_UNORM;
    ID3D11VideoContext_VideoProcessorSetStreamFrameFormat(g->video_context,g->processor,0,D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE);
    ID3D11VideoContext_VideoProcessorSetStreamAutoProcessingMode(g->video_context,g->processor,0,FALSE);
    ID3D11VideoContext_VideoProcessorSetStreamSourceRect(g->video_context,g->processor,0,TRUE,&src);
    ID3D11VideoContext_VideoProcessorSetStreamDestRect(g->video_context,g->processor,0,TRUE,&dst);
    ID3D11VideoContext_VideoProcessorSetOutputTargetRect(g->video_context,g->processor,TRUE,&dst);
    ID3D11VideoContext_VideoProcessorSetStreamColorSpace(g->video_context,g->processor,0,source_rgb?&rgb:&color);
    ID3D11VideoContext_VideoProcessorSetOutputColorSpace(g->video_context,g->processor,source_rgb?&color:&rgb);
    D3D11_VIDEO_PROCESSOR_STREAM stream={0};stream.Enable=TRUE;stream.pInputSurface=input;
    result=ID3D11VideoContext_VideoProcessorBlt(g->video_context,g->processor,output,0,1,&stream);
    ID3D11VideoProcessorOutputView_Release(output);ID3D11VideoProcessorInputView_Release(input);
    // Both the capture and codec use this protected immediate context. Flushing
    // orders use of decoder pool surfaces before their references are released.
    ID3D11DeviceContext_Flush(g->context);
    return FAILED(result)?-1:0;
}
int frames_gpu_readback(void *opaque,void *source,int slice,int width,int height,uint8_t *bytes) {
    GpuConverter *g=opaque;if(!g || width!=g->width || height!=g->height)return -1;
    if(!g->readback){
        D3D11_TEXTURE2D_DESC desc={0};desc.Width=width;desc.Height=height;desc.MipLevels=desc.ArraySize=1;desc.Format=DXGI_FORMAT_B8G8R8A8_UNORM;desc.SampleDesc.Count=1;
        desc.Usage=D3D11_USAGE_DEFAULT;desc.BindFlags=D3D11_BIND_RENDER_TARGET|D3D11_BIND_SHADER_RESOURCE;
        if(FAILED(ID3D11Device_CreateTexture2D(g->device,&desc,NULL,&g->rgb)))return -1;
        desc.Usage=D3D11_USAGE_STAGING;desc.BindFlags=0;desc.CPUAccessFlags=D3D11_CPU_ACCESS_READ;
        if(FAILED(ID3D11Device_CreateTexture2D(g->device,&desc,NULL,&g->readback)))return -1;
    }
    D3D11_TEXTURE2D_DESC desc;ID3D11Texture2D_GetDesc((ID3D11Texture2D *)source,&desc);
    if(desc.Format==DXGI_FORMAT_B8G8R8A8_UNORM)ID3D11DeviceContext_CopySubresourceRegion(g->context,(ID3D11Resource *)g->readback,0,0,0,0,(ID3D11Resource *)source,slice,NULL);
    else {if(frames_gpu_convert(g,source,slice,g->rgb,0,width,height,1,0))return -1;ID3D11DeviceContext_CopyResource(g->context,(ID3D11Resource *)g->readback,(ID3D11Resource *)g->rgb);}
    D3D11_MAPPED_SUBRESOURCE mapped;
    if(FAILED(ID3D11DeviceContext_Map(g->context,(ID3D11Resource *)g->readback,0,D3D11_MAP_READ,0,&mapped)))return -1;
    for(int y=0;y<height;y++)memcpy(bytes+(size_t)y*width*4,(uint8_t *)mapped.pData+(size_t)y*mapped.RowPitch,(size_t)width*4);
    ID3D11DeviceContext_Unmap(g->context,(ID3D11Resource *)g->readback,0);return 0;
}
Media *frames_encoder_open_gpu(const char *name,int width,int height,int fps,int bitrate,void *source,char *error,int capacity) {
    const AVCodec *codec=p_avcodec_find_encoder_by_name(name);if(!codec)return NULL;
    int supported=0;
    for(int i=0;;i++){const AVCodecHWConfig *cfg=p_avcodec_get_hw_config(codec,i);if(!cfg)break;if(cfg->pix_fmt==AV_PIX_FMT_D3D11 && (cfg->methods&AV_CODEC_HW_CONFIG_METHOD_HW_FRAMES_CTX))supported=1;}
    if(!supported){snprintf(error,capacity,"Encoder does not accept D3D11 surfaces");return NULL;}
    Media *m=create(codec,width,height);if(!m)return NULL;
    AVCodecContext *c=m->ctx;m->padded_width=(width+1)&~1;m->padded_height=(height+1)&~1;
    c->width=m->padded_width;c->height=m->padded_height;c->time_base=(AVRational){1,fps};c->framerate=(AVRational){fps,1};
    c->pix_fmt=AV_PIX_FMT_D3D11;c->bit_rate=c->rc_max_rate=bitrate;c->rc_buffer_size=bitrate/m->vbv_divisor;c->max_b_frames=0;c->gop_size=m->gop;c->flags|=AV_CODEC_FLAG_LOW_DELAY;
    c->colorspace=AVCOL_SPC_BT709;c->color_range=AVCOL_RANGE_MPEG;
    if(strstr(name,"_nvenc")){option(m,"rc","cbr");option(m,"preset","p1");option(m,"tune","ull");option(m,"zerolatency","1");option(m,"delay","0");option(m,"rc-lookahead","0");option(m,"forced-idr","1");}
    if(strstr(name,"_amf")){option(m,"usage","ultralowlatency");option(m,"quality","speed");option(m,"async_depth","1");option(m,"preanalysis","0");}
    c->hw_device_ctx=p_av_hwdevice_ctx_alloc(AV_HWDEVICE_TYPE_D3D11VA);if(!c->hw_device_ctx)goto fail;
    AVHWDeviceContext *device=(AVHWDeviceContext *)c->hw_device_ctx->data;AVD3D11VADeviceContext *hw=device->hwctx;
    ID3D11Texture2D_GetDevice((ID3D11Texture2D *)source,&hw->device);
    if(p_av_hwdevice_ctx_init(c->hw_device_ctx)<0)goto fail;
    c->hw_frames_ctx=p_av_hwframe_ctx_alloc(c->hw_device_ctx);if(!c->hw_frames_ctx)goto fail;
    AVHWFramesContext *frames=(AVHWFramesContext *)c->hw_frames_ctx->data;
    frames->format=AV_PIX_FMT_D3D11;frames->sw_format=AV_PIX_FMT_NV12;frames->width=c->width;frames->height=c->height;frames->initial_pool_size=0;
    ((AVD3D11VAFramesContext *)frames->hwctx)->BindFlags=D3D11_BIND_RENDER_TARGET;
    if(p_av_hwframe_ctx_init(c->hw_frames_ctx)<0)goto fail;
    if(p_avcodec_open2(c,codec,NULL)<0)goto fail;
    m->gpu_converter=frames_gpu_converter_open(source,c->width,c->height);if(!m->gpu_converter)goto fail;
    m->gpu_encoder=1;return m;
fail: snprintf(error,capacity,"D3D11 encoder interoperability probe failed");frames_media_close(m);return NULL;
}
int frames_encoder_send_gpu(Media *m,void *source,int slice,int width,int height,char *error,int capacity) {
    p_av_frame_unref(m->converted);
    int result=p_av_hwframe_get_buffer(m->ctx->hw_frames_ctx,m->converted,0);if(result<0)return failure(result,error,capacity);
    if(frames_gpu_convert(m->gpu_converter,source,slice,m->converted->data[0],(int)(intptr_t)m->converted->data[1],width,height,1,0)){
        snprintf(error,capacity,"D3D11 color conversion failed");return -1;
    }
    m->converted->pts=m->pts++;m->converted->pict_type=m->force_keyframe?AV_PICTURE_TYPE_I:AV_PICTURE_TYPE_NONE;m->force_keyframe=0;
    return failure(p_avcodec_send_frame(m->ctx,m->converted),error,capacity);
}
#else
void frames_gpu_converter_close(void *opaque) { (void)opaque; }
#endif

void frames_decoder_flush(Media *m) { p_avcodec_flush_buffers(m->ctx);p_av_frame_unref(m->frame);p_av_frame_unref(m->cpu); }
