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
#include <windows.h>
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
 F(int, avcodec_send_packet, (AVCodecContext *, const AVPacket *)) \
 F(int, avcodec_receive_frame, (AVCodecContext *, AVFrame *)) \
 F(const AVCodecHWConfig *, avcodec_get_hw_config, (const AVCodec *, int)) \
 F(AVPacket *, av_packet_alloc, (void)) \
 F(void, av_packet_free, (AVPacket **)) \
 F(void, av_packet_unref, (AVPacket *)) \
 F(int, av_new_packet, (AVPacket *, int)) \
 F(AVFrame *, av_frame_alloc, (void)) \
 F(void, av_frame_free, (AVFrame **)) \
 F(void, av_frame_unref, (AVFrame *)) \
 F(int, av_frame_get_buffer, (AVFrame *, int)) \
 F(int, av_frame_make_writable, (AVFrame *)) \
 F(int, av_opt_set, (void *, const char *, const char *, int)) \
 F(int, av_opt_set_int, (void *, const char *, int64_t, int)) \
 F(int, av_strerror, (int, char *, size_t)) \
 F(void, av_log_set_level, (int)) \
 F(int, av_hwdevice_ctx_create, (AVBufferRef **, enum AVHWDeviceType, const char *, AVDictionary *, int)) \
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

typedef struct Media {
    AVCodecContext *ctx;
    AVFrame *frame, *cpu, *converted;
    AVPacket *packet;
    struct SwsContext *sws;
    enum AVPixelFormat hw_format;
    int width, height, padded_width, padded_height;
    int64_t pts;
    uint8_t *padded;
#ifdef __APPLE__
    VTCompressionSessionRef vt;
    dispatch_semaphore_t ready;
    uint8_t *vt_bytes;
    int vt_size, vt_key, vt_error, vt_received, fps;
#endif
} Media;

void frames_media_close(Media *m) {
    if (!m) return;
#ifdef __APPLE__
    if (m->vt) { VTCompressionSessionInvalidate(m->vt); CFRelease(m->vt); }
    if (m->ready) dispatch_release(m->ready);
    free(m->vt_bytes);
#endif
    p_avcodec_free_context(&m->ctx);
    p_av_frame_free(&m->frame);
    p_av_frame_free(&m->cpu);
    p_av_frame_free(&m->converted);
    p_av_packet_free(&m->packet);
    p_sws_freeContext(m->sws);
    free(m->padded);
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
    } else if (!status) m->vt_error=-1;
    dispatch_semaphore_signal(m->ready);
}
static int vt_open(Media *m,const char *name,int fps) {
    int hevc=strstr(name,"hevc")!=NULL;m->fps=fps;
    const void *keys[]={kVTVideoEncoderSpecification_RequireHardwareAcceleratedVideoEncoder,kVTVideoEncoderSpecification_EnableLowLatencyRateControl};
    const void *values[]={kCFBooleanTrue,kCFBooleanTrue};
    CFDictionaryRef spec=CFDictionaryCreate(NULL,keys,values,2,&kCFTypeDictionaryKeyCallBacks,&kCFTypeDictionaryValueCallBacks);
    OSStatus result=VTCompressionSessionCreate(NULL,m->padded_width,m->padded_height,
        hevc?kCMVideoCodecType_HEVC:kCMVideoCodecType_H264,spec,NULL,NULL,vt_output,m,&m->vt);
    CFRelease(spec);if(result)return result;
    m->ready=dispatch_semaphore_create(0);if(!m->ready)return -1;
    if (VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_RealTime,kCFBooleanTrue) ||
        VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_AllowFrameReordering,kCFBooleanFalse)) return -1;
    int bitrate=12000000,gop=60;
    CFNumberRef number=CFNumberCreate(NULL,kCFNumberIntType,&bitrate);
    result=VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_AverageBitRate,number);CFRelease(number);if(result)return result;
    number=CFNumberCreate(NULL,kCFNumberIntType,&fps);
    VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_ExpectedFrameRate,number);CFRelease(number);
    number=CFNumberCreate(NULL,kCFNumberIntType,&gop);
    VTSessionSetProperty(m->vt,kVTCompressionPropertyKey_MaxKeyFrameInterval,number);CFRelease(number);
    return VTCompressionSessionPrepareToEncodeFrames(m->vt);
}
static int vt_send(Media *m,const uint8_t *bgra) {
    CVPixelBufferRef buffer=NULL;
    OSStatus result=CVPixelBufferCreate(NULL,m->padded_width,m->padded_height,kCVPixelFormatType_32BGRA,NULL,&buffer);
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
    result=VTCompressionSessionEncodeFrame(m->vt,buffer,CMTimeMake(m->pts++,m->fps),CMTimeMake(1,m->fps),NULL,NULL,NULL);
    CVPixelBufferRelease(buffer);
    if(result)return result;
    if(dispatch_semaphore_wait(m->ready,dispatch_time(DISPATCH_TIME_NOW,2*NSEC_PER_SEC)))return -1;
    return m->vt_error;
}
#endif

Media *frames_encoder_open(const char *name, int width, int height, int fps, char *error, int capacity) {
    const AVCodec *codec = p_avcodec_find_encoder_by_name(name);
    Media *m = create(codec, width, height);
    if (!m) { snprintf(error, capacity, "Encoder %s unavailable", name); return NULL; }
    AVCodecContext *c = m->ctx;
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
    c->bit_rate = 12000000; c->gop_size = 60; c->max_b_frames = 0;
    c->flags |= AV_CODEC_FLAG_LOW_DELAY;
    c->thread_count = 2; c->thread_type = FF_THREAD_SLICE;
    c->pix_fmt = AV_PIX_FMT_YUV420P;
    if (strstr(name, "_amf") || strstr(name, "_qsv")) c->pix_fmt = AV_PIX_FMT_NV12;
    if (strstr(name, "_nvenc")) {
        option(m,"preset","p1"); option(m,"tune","ull"); option(m,"zerolatency","1");
        option(m,"rc-lookahead","0"); option(m,"delay","0");
    } else if (strstr(name,"_amf")) {
        option(m,"usage","ultralowlatency"); option(m,"quality","speed");
        option(m,"preanalysis","0"); option(m,"async_depth","1");
    } else if (strstr(name,"_qsv")) {
        option(m,"preset","veryfast"); option(m,"async_depth","1"); option(m,"look_ahead","0");
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
    return failure(p_avcodec_send_frame(m->ctx,m->converted),error,capacity);
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
