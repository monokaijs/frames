// Phase 0 spike 0-C: drive NVENC directly (no FFmpeg) from D3D11 textures and
// measure the low-latency features Frames v2 needs.
//
// Content: a 2x-size high-frequency texture scrolled on the GPU every frame
// (real motion for motion estimation) plus a moving solid box, copied into a
// BGRA texture that NVENC reads directly (NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX).
//
// Scheduled events (frame index):
//   RFI_SINGLE  invalidate the previous frame (simulated single lost frame)
//   RFI_BURST   invalidate the previous three frames
//   FORCE_IDR   NV_ENC_PIC_FLAG_FORCEIDR for size comparison
//   FORCE_IR    forceIntraRefreshWithFrameCnt (on-demand gradual refresh)
// The "lossy" output omits exactly the frames whose loss each event repairs,
// so decoding it and comparing frame hashes with the full stream proves the
// recovery is bit-exact.
//
// Build (MSYS2 UCRT64): g++ -O2 -std=c++17 -static nvenc_spike.cpp -o nvenc_spike.exe -ld3d11 -ldxgi -lwinmm

#include <windows.h>
#include <d3d11_1.h>
#include <mmsystem.h>

#include <algorithm>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <set>
#include <string>
#include <vector>

#include "third_party/nvEncodeAPI.h"

static NV_ENCODE_API_FUNCTION_LIST nv = {NV_ENCODE_API_FUNCTION_LIST_VER};
static void *encoder = nullptr;

#define CK(expr)                                                                          \
    do {                                                                                  \
        NVENCSTATUS status_ = (expr);                                                     \
        if (status_ != NV_ENC_SUCCESS) {                                                  \
            fprintf(stderr, "%s failed: %d %s\n", #expr, (int)status_,                     \
                    encoder && nv.nvEncGetLastErrorString ? nv.nvEncGetLastErrorString(encoder) : ""); \
            exit(1);                                                                      \
        }                                                                                 \
    } while (0)
#define HR(expr)                                                                          \
    do {                                                                                  \
        HRESULT hr_ = (expr);                                                             \
        if (FAILED(hr_)) {                                                                \
            fprintf(stderr, "%s failed: 0x%08lx\n", #expr, (unsigned long)hr_);           \
            exit(1);                                                                      \
        }                                                                                 \
    } while (0)

struct Options {
    bool hevc = false;
    uint32_t width = 1920, height = 1080, fps = 60, frames = 900, mbps = 30, slices = 4;
    bool subframe = true;
    std::string intra_refresh = "ondemand";  // off | ondemand | periodic
    std::string out = "out";
    bool trace = false;
    uint32_t detail = 1;
    uint32_t burn_seconds = 0;  // pattern cell scale: 1 = worst-case noise, 8+ = game/desktop-like
};

struct Record {
    uint32_t frame;
    int type;
    uint32_t bytes, slices, qp;
    double first_us, done_us;
    int polls;
};

static double qpc_us() {
    static LARGE_INTEGER frequency = [] { LARGE_INTEGER f; QueryPerformanceFrequency(&f); return f; }();
    LARGE_INTEGER now;
    QueryPerformanceCounter(&now);
    return (double)now.QuadPart * 1e6 / (double)frequency.QuadPart;
}

static double percentile(std::vector<double> values, double p) {
    if (values.empty()) return 0;
    std::sort(values.begin(), values.end());
    return values[(size_t)((values.size() - 1) * p)];
}

static const char *type_name(int type) {
    switch (type) {
        case NV_ENC_PIC_TYPE_P: return "P";
        case NV_ENC_PIC_TYPE_I: return "I";
        case NV_ENC_PIC_TYPE_IDR: return "IDR";
        case NV_ENC_PIC_TYPE_INTRA_REFRESH: return "IR";
        default: return "?";
    }
}

static uint32_t cap(GUID codec, NV_ENC_CAPS which) {
    NV_ENC_CAPS_PARAM param = {NV_ENC_CAPS_PARAM_VER};
    param.capsToQuery = which;
    int value = 0;
    if (nv.nvEncGetEncodeCaps(encoder, codec, &param, &value) != NV_ENC_SUCCESS) return 0xffffffff;
    return (uint32_t)value;
}

int main(int argc, char **argv) {
    Options o;
    for (int i = 1; i < argc; i++) {
        std::string a = argv[i];
        auto next = [&] { return std::string(i + 1 < argc ? argv[++i] : ""); };
        if (a == "--hevc") o.hevc = true;
        else if (a == "--size") { std::string s = next(); sscanf(s.c_str(), "%ux%u", &o.width, &o.height); }
        else if (a == "--fps") o.fps = std::stoul(next());
        else if (a == "--frames") o.frames = std::stoul(next());
        else if (a == "--mbps") o.mbps = std::stoul(next());
        else if (a == "--slices") o.slices = std::stoul(next());
        else if (a == "--no-subframe") o.subframe = false;
        else if (a == "--ir") o.intra_refresh = next();
        else if (a == "--out") o.out = next();
        else if (a == "--trace") o.trace = true;
        else if (a == "--burn") o.burn_seconds = std::stoul(next());
        else if (a == "--detail") o.detail = std::max(1ul, std::stoul(next()));
        else { fprintf(stderr, "unknown option %s\n", a.c_str()); return 2; }
    }
    const uint32_t RFI_SINGLE = 300, RFI_BURST = 360, FORCE_IDR = 450, FORCE_IR = 600;
    const uint32_t IR_FRAMES = std::max(2u, o.fps / 6);
    timeBeginPeriod(1);

    // ---- D3D11 device and content ----
    ID3D11Device *device = nullptr;
    ID3D11DeviceContext *context = nullptr;
    D3D_FEATURE_LEVEL level;
    // The default adapter can be an iGPU (on the test PC it is the AMD Radeon),
    // so pick the NVIDIA adapter explicitly: NVENC only opens on its own device.
    IDXGIFactory1 *factory = nullptr;
    HR(CreateDXGIFactory1(__uuidof(IDXGIFactory1), (void **)&factory));
    IDXGIAdapter1 *nvidia = nullptr;
    for (UINT i = 0;; i++) {
        IDXGIAdapter1 *candidate = nullptr;
        if (factory->EnumAdapters1(i, &candidate) == DXGI_ERROR_NOT_FOUND) break;
        DXGI_ADAPTER_DESC1 desc;
        candidate->GetDesc1(&desc);
        printf("adapter %u: %ls vendor=0x%04x\n", i, desc.Description, desc.VendorId);
        if (desc.VendorId == 0x10de && !nvidia) nvidia = candidate;
        else candidate->Release();
    }
    if (!nvidia) { fprintf(stderr, "no NVIDIA adapter\n"); return 1; }
    HR(D3D11CreateDevice(nvidia, D3D_DRIVER_TYPE_UNKNOWN, nullptr, 0, nullptr, 0, D3D11_SDK_VERSION,
                         &device, &level, &context));
    ID3D11DeviceContext1 *context1 = nullptr;
    HR(context->QueryInterface(__uuidof(ID3D11DeviceContext1), (void **)&context1));
    if (o.burn_seconds) {
        // GPU load generator (separate process) to test whether NVENC latency
        // depends on the GPU power state: large clears keep the 3D engine busy.
        D3D11_TEXTURE2D_DESC bd = {};
        bd.Width = 4096; bd.Height = 4096; bd.MipLevels = 1; bd.ArraySize = 1;
        bd.Format = DXGI_FORMAT_R32G32B32A32_FLOAT; bd.SampleDesc.Count = 1;
        bd.Usage = D3D11_USAGE_DEFAULT; bd.BindFlags = D3D11_BIND_RENDER_TARGET;
        ID3D11Texture2D *burn = nullptr;
        ID3D11RenderTargetView *view = nullptr;
        HR(device->CreateTexture2D(&bd, nullptr, &burn));
        HR(device->CreateRenderTargetView(burn, nullptr, &view));
        double stop = qpc_us() + o.burn_seconds * 1e6;
        uint64_t clears = 0;
        while (qpc_us() < stop) {
            float c[4] = {(float)(clears % 7), 1, 0, 1};
            for (int k = 0; k < 8; k++) context->ClearRenderTargetView(view, c);
            context->Flush();
            clears += 8;
            Sleep(1);
        }
        printf("burn done: %llu clears\n", (unsigned long long)clears);
        return 0;
    }
    {
        IDXGIDevice *dxgi = nullptr;
        IDXGIAdapter *adapter = nullptr;
        DXGI_ADAPTER_DESC desc;
        device->QueryInterface(__uuidof(IDXGIDevice), (void **)&dxgi);
        dxgi->GetAdapter(&adapter);
        adapter->GetDesc(&desc);
        printf("adapter: %ls\n", desc.Description);
        adapter->Release();
        dxgi->Release();
    }

    const uint32_t W = o.width, H = o.height;
    std::vector<uint32_t> pattern((size_t)W * 2 * H * 2);
    for (uint32_t y = 0; y < H * 2; y++)
        for (uint32_t x = 0; x < W * 2; x++) {
            // Text-like high-frequency cells over a smooth gradient.
            uint32_t h = (x / (3 * o.detail)) * 73856093u ^ (y / (5 * o.detail)) * 19349663u;
            h ^= h >> 13; h *= 0x5bd1e995u; h ^= h >> 15;
            uint8_t ink = (h & 7) < 2 ? 30 : 220;
            uint8_t r = (uint8_t)((x * 255) / (W * 2)), g = (uint8_t)((y * 255) / (H * 2));
            uint8_t b = ink;
            pattern[(size_t)y * W * 2 + x] = 0xff000000u | (r << 16) | (g << 8) | b;
        }
    D3D11_TEXTURE2D_DESC td = {};
    td.Width = W * 2; td.Height = H * 2; td.MipLevels = 1; td.ArraySize = 1;
    td.Format = DXGI_FORMAT_B8G8R8A8_UNORM; td.SampleDesc.Count = 1; td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_SHADER_RESOURCE;
    D3D11_SUBRESOURCE_DATA init = {pattern.data(), W * 2 * 4, 0};
    ID3D11Texture2D *source = nullptr;
    HR(device->CreateTexture2D(&td, &init, &source));
    pattern.clear();
    pattern.shrink_to_fit();

    const int RING = 3;
    ID3D11Texture2D *frames[RING];
    ID3D11RenderTargetView *rtv[RING];
    td.Width = W; td.Height = H; td.BindFlags = D3D11_BIND_RENDER_TARGET;
    for (int i = 0; i < RING; i++) {
        HR(device->CreateTexture2D(&td, nullptr, &frames[i]));
        HR(device->CreateRenderTargetView(frames[i], nullptr, &rtv[i]));
    }
    D3D11_QUERY_DESC qd = {D3D11_QUERY_EVENT, 0};
    ID3D11Query *gpu_done = nullptr;
    HR(device->CreateQuery(&qd, &gpu_done));

    // ---- NVENC ----
    HMODULE lib = LoadLibraryA("nvEncodeAPI64.dll");
    if (!lib) { fprintf(stderr, "nvEncodeAPI64.dll not found\n"); return 1; }
    typedef NVENCSTATUS(NVENCAPI * MaxVersion)(uint32_t *);
    typedef NVENCSTATUS(NVENCAPI * CreateInstance)(NV_ENCODE_API_FUNCTION_LIST *);
    auto max_version = (MaxVersion)GetProcAddress(lib, "NvEncodeAPIGetMaxSupportedVersion");
    auto create_instance = (CreateInstance)GetProcAddress(lib, "NvEncodeAPICreateInstance");
    uint32_t driver_version = 0;
    CK(max_version(&driver_version));
    uint32_t header_version = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
    printf("NVENC API: driver supports %u.%u, header %u.%u\n", driver_version >> 4, driver_version & 15,
           NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION);
    if (header_version > driver_version) { fprintf(stderr, "driver too old for header\n"); return 1; }
    CK(create_instance(&nv));

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS session = {NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    session.device = device;
    session.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    session.apiVersion = NVENCAPI_VERSION;
    CK(nv.nvEncOpenEncodeSessionEx(&session, &encoder));

    GUID codec = o.hevc ? NV_ENC_CODEC_HEVC_GUID : NV_ENC_CODEC_H264_GUID;
    printf("caps: subframe_readback=%u intra_refresh=%u ref_pic_invalidation=%u max_ltr=%u custom_vbv=%u "
           "multiple_ref=%u dyn_bitrate=%u dyn_res=%u single_slice_ir=%u engines=%u yuv444=%u 10bit=%u\n",
           cap(codec, NV_ENC_CAPS_SUPPORT_SUBFRAME_READBACK), cap(codec, NV_ENC_CAPS_SUPPORT_INTRA_REFRESH),
           cap(codec, NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION), cap(codec, NV_ENC_CAPS_NUM_MAX_LTR_FRAMES),
           cap(codec, NV_ENC_CAPS_SUPPORT_CUSTOM_VBV_BUF_SIZE), cap(codec, NV_ENC_CAPS_SUPPORT_MULTIPLE_REF_FRAMES),
           cap(codec, NV_ENC_CAPS_SUPPORT_DYN_BITRATE_CHANGE), cap(codec, NV_ENC_CAPS_SUPPORT_DYN_RES_CHANGE),
           cap(codec, NV_ENC_CAPS_SINGLE_SLICE_INTRA_REFRESH), cap(codec, NV_ENC_CAPS_NUM_ENCODER_ENGINES),
           cap(codec, NV_ENC_CAPS_SUPPORT_YUV444_ENCODE), cap(codec, NV_ENC_CAPS_SUPPORT_10BIT_ENCODE));

    NV_ENC_PRESET_CONFIG preset = {NV_ENC_PRESET_CONFIG_VER, 0, {NV_ENC_CONFIG_VER}};
    CK(nv.nvEncGetEncodePresetConfigEx(encoder, codec, NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY,
                                        &preset));
    NV_ENC_CONFIG config = preset.presetCfg;
    config.version = NV_ENC_CONFIG_VER;
    config.gopLength = NVENC_INFINITE_GOPLENGTH;
    config.frameIntervalP = 1;
    config.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config.rcParams.averageBitRate = o.mbps * 1000000;
    config.rcParams.maxBitRate = o.mbps * 1000000;
    config.rcParams.vbvBufferSize = o.mbps * 1000000 / o.fps;  // one frame
    config.rcParams.vbvInitialDelay = config.rcParams.vbvBufferSize;
    config.rcParams.zeroReorderDelay = 1;
    config.rcParams.enableLookahead = 0;
    bool ir = o.intra_refresh != "off";
    uint32_t ir_period = o.intra_refresh == "periodic" ? o.fps : 1000000;  // "ondemand": effectively never periodic
    if (o.hevc) {
        auto &c = config.encodeCodecConfig.hevcConfig;
        c.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        c.repeatSPSPPS = 1;
        c.maxNumRefFramesInDPB = 4;
        c.sliceMode = 3;
        c.sliceModeData = o.slices;
        c.enableIntraRefresh = ir;
        c.intraRefreshPeriod = ir_period;
        c.intraRefreshCnt = IR_FRAMES;
    } else {
        auto &c = config.encodeCodecConfig.h264Config;
        c.idrPeriod = NVENC_INFINITE_GOPLENGTH;
        c.repeatSPSPPS = 1;
        c.maxNumRefFrames = 4;
        c.sliceMode = 3;
        c.sliceModeData = o.slices;
        c.enableIntraRefresh = ir;
        c.intraRefreshPeriod = ir_period;
        c.intraRefreshCnt = IR_FRAMES;
    }

    NV_ENC_INITIALIZE_PARAMS initialize = {NV_ENC_INITIALIZE_PARAMS_VER};
    initialize.encodeGUID = codec;
    initialize.presetGUID = NV_ENC_PRESET_P1_GUID;
    initialize.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    initialize.encodeWidth = initialize.darWidth = initialize.maxEncodeWidth = W;
    initialize.encodeHeight = initialize.darHeight = initialize.maxEncodeHeight = H;
    initialize.frameRateNum = o.fps;
    initialize.frameRateDen = 1;
    initialize.enablePTD = 1;
    initialize.enableEncodeAsync = 0;
    initialize.reportSliceOffsets = 1;
    initialize.enableSubFrameWrite = o.subframe;
    initialize.encodeConfig = &config;
    CK(nv.nvEncInitializeEncoder(encoder, &initialize));

    NV_ENC_REGISTERED_PTR registered[RING];
    for (int i = 0; i < RING; i++) {
        NV_ENC_REGISTER_RESOURCE r = {NV_ENC_REGISTER_RESOURCE_VER};
        r.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        r.width = W;
        r.height = H;
        r.resourceToRegister = frames[i];
        r.bufferFormat = NV_ENC_BUFFER_FORMAT_ARGB;
        r.bufferUsage = NV_ENC_INPUT_IMAGE;
        CK(nv.nvEncRegisterResource(encoder, &r));
        registered[i] = r.registeredResource;
    }
    NV_ENC_CREATE_BITSTREAM_BUFFER bitstream = {NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    CK(nv.nvEncCreateBitstreamBuffer(encoder, &bitstream));

    std::string ext = o.hevc ? ".hevc" : ".h264";
    FILE *full = fopen((o.out + ext).c_str(), "wb");
    FILE *lossy = fopen((o.out + "-lossy" + ext).c_str(), "wb");
    FILE *csv = fopen((o.out + ".csv").c_str(), "w");
    fprintf(csv, "frame,type,bytes,slices,qp,first_us,done_us,polls\n");
    std::set<uint32_t> dropped = {RFI_SINGLE - 1, RFI_BURST - 3, RFI_BURST - 2, RFI_BURST - 1};
    if (ir) dropped.insert(FORCE_IR - 1);
    dropped.insert(FORCE_IDR - 1);

    std::vector<uint32_t> slice_offsets(4096);
    std::vector<Record> records;
    const double interval = 1e6 / o.fps;
    double next = qpc_us();
    bool traced = false;
    for (uint32_t f = 0; f < o.frames; f++) {
        next += interval;
        while (qpc_us() < next - 1500) Sleep(1);
        while (qpc_us() < next) YieldProcessor();

        int slot = f % RING;
        D3D11_BOX box = {(f * 7) % W, (f * 3) % H, 0, (f * 7) % W + W, (f * 3) % H + H, 1};
        context->CopySubresourceRegion(frames[slot], 0, 0, 0, 0, source, 0, &box);
        float color[4] = {(f % 60) / 60.0f, 0.2f, 1.0f - (f % 90) / 90.0f, 1};
        D3D11_RECT rect = {(LONG)((f * 11) % (W - 200)), (LONG)((f * 5) % (H - 200)), 0, 0};
        rect.right = rect.left + 200;
        rect.bottom = rect.top + 200;
        context1->ClearView(rtv[slot], color, &rect, 1);
        context->End(gpu_done);
        context->Flush();
        while (context->GetData(gpu_done, nullptr, 0, 0) == S_FALSE) YieldProcessor();

        if (f == RFI_SINGLE) CK(nv.nvEncInvalidateRefFrames(encoder, f - 1));
        if (f == RFI_BURST)
            for (uint32_t lost = f - 3; lost < f; lost++) CK(nv.nvEncInvalidateRefFrames(encoder, lost));

        double t0 = qpc_us();
        NV_ENC_MAP_INPUT_RESOURCE map = {NV_ENC_MAP_INPUT_RESOURCE_VER};
        map.registeredResource = registered[slot];
        CK(nv.nvEncMapInputResource(encoder, &map));
        NV_ENC_PIC_PARAMS pic = {NV_ENC_PIC_PARAMS_VER};
        pic.inputWidth = W;
        pic.inputHeight = H;
        pic.inputBuffer = map.mappedResource;
        pic.bufferFmt = map.mappedBufferFmt;
        pic.outputBitstream = bitstream.bitstreamBuffer;
        pic.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
        pic.inputTimeStamp = f;
        pic.frameIdx = f;
        if (f == FORCE_IDR) pic.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        if (f == FORCE_IR && ir) {
            if (o.hevc) pic.codecPicParams.hevcPicParams.forceIntraRefreshWithFrameCnt = IR_FRAMES;
            else pic.codecPicParams.h264PicParams.forceIntraRefreshWithFrameCnt = IR_FRAMES;
        }
        CK(nv.nvEncEncodePicture(encoder, &pic));

        NV_ENC_LOCK_BITSTREAM lock = {NV_ENC_LOCK_BITSTREAM_VER};
        lock.outputBitstream = bitstream.bitstreamBuffer;
        lock.sliceOffsets = slice_offsets.data();
        lock.doNotWait = o.subframe;
        double first = 0;
        int polls = 0;
        bool trace = o.trace && o.subframe && !traced && f == 100;
        for (;;) {
            NVENCSTATUS status = nv.nvEncLockBitstream(encoder, &lock);
            polls++;
            if (status == NV_ENC_ERR_LOCK_BUSY) { YieldProcessor(); continue; }
            CK(status);
            double now = qpc_us();
            if (trace)
                printf("  poll %d t=%.1fus size=%u slices=%u hwEncodeStatus=%u\n", polls, now - t0,
                       lock.bitstreamSizeInBytes, lock.numSlices, lock.hwEncodeStatus);
            if (first == 0 && lock.bitstreamSizeInBytes > 0) first = now - t0;
            // hwEncodeStatus 2 marks a completed picture in sub-frame mode (observed); a
            // blocking lock always returns complete pictures.
            bool done = !o.subframe || lock.hwEncodeStatus == 2;
            if (done) {
                Record r = {f, (int)lock.pictureType, lock.bitstreamSizeInBytes, lock.numSlices, lock.frameAvgQP,
                            first, now - t0, polls};
                records.push_back(r);
                fwrite(lock.bitstreamBufferPtr, 1, lock.bitstreamSizeInBytes, full);
                if (!dropped.count(f)) fwrite(lock.bitstreamBufferPtr, 1, lock.bitstreamSizeInBytes, lossy);
                CK(nv.nvEncUnlockBitstream(encoder, bitstream.bitstreamBuffer));
                break;
            }
            CK(nv.nvEncUnlockBitstream(encoder, bitstream.bitstreamBuffer));
            YieldProcessor();
        }
        if (trace) traced = true;
        CK(nv.nvEncUnmapInputResource(encoder, map.mappedResource));
    }

    for (auto &r : records)
        fprintf(csv, "%u,%s,%u,%u,%u,%.1f,%.1f,%d\n", r.frame, type_name(r.type), r.bytes, r.slices, r.qp, r.first_us,
                r.done_us, r.polls);
    fclose(csv);
    fclose(full);
    fclose(lossy);

    std::vector<double> first, done, sizes;
    std::set<uint32_t> special = {0, RFI_SINGLE, RFI_BURST, FORCE_IDR, FORCE_IR};
    for (auto &r : records) {
        if (r.frame < 30) continue;
        first.push_back(r.first_us);
        done.push_back(r.done_us);
        if (!special.count(r.frame)) sizes.push_back(r.bytes);
    }
    double median = percentile(sizes, 0.5);
    printf("%s %ux%u@%u %u Mbps slices=%u subframe=%d ir=%s detail=%u frames=%u\n", o.hevc ? "HEVC" : "H264", W, H,
           o.fps, o.mbps, o.slices, (int)o.subframe, o.intra_refresh.c_str(), o.detail, o.frames);
    printf("  first-slice us p50 %.0f p95 %.0f p99 %.0f | full AU us p50 %.0f p95 %.0f p99 %.0f max %.0f\n",
           percentile(first, .5), percentile(first, .95), percentile(first, .99), percentile(done, .5),
           percentile(done, .95), percentile(done, .99), percentile(done, 1));
    printf("  size bytes p50 %.0f p95 %.0f max %.0f (VBV %u)\n", median, percentile(sizes, .95), percentile(sizes, 1),
           config.rcParams.vbvBufferSize / 8);
    int spikes = 0;
    for (auto &r : records)
        if (r.frame >= 30 && !special.count(r.frame) && r.bytes > 3 * median) spikes++;
    int counts[16] = {};
    for (auto &r : records) counts[r.type & 15]++;
    printf("  types: P=%d I=%d IDR=%d IR=%d | frames >3x median (excluding events): %d\n", counts[NV_ENC_PIC_TYPE_P],
           counts[NV_ENC_PIC_TYPE_I], counts[NV_ENC_PIC_TYPE_IDR], counts[NV_ENC_PIC_TYPE_INTRA_REFRESH], spikes);
    for (uint32_t e : {0u, RFI_SINGLE, RFI_BURST, FORCE_IDR, FORCE_IR}) {
        if (e >= records.size()) continue;
        printf("  event@%u:", e);
        for (uint32_t k = e > 0 ? e - 1 : 0; k < std::min<uint32_t>(e + 4, records.size()); k++)
            printf(" [%u %s %uB %.0fus]", k, type_name(records[k].type), records[k].bytes, records[k].done_us);
        printf("\n");
    }
    printf("  dropped in lossy stream:");
    for (auto d : dropped) printf(" %u", d);
    printf("\n");

    nv.nvEncDestroyBitstreamBuffer(encoder, bitstream.bitstreamBuffer);
    for (int i = 0; i < RING; i++) nv.nvEncUnregisterResource(encoder, registered[i]);
    nv.nvEncDestroyEncoder(encoder);
    return 0;
}
