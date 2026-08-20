/*
 * A minimal in-process camera that speaks the Touptek API.
 *
 * It exists to exercise the crate's FFI layer — the event callback, the pull
 * path, teardown ordering — without the vendor SDK or hardware. It models
 * only what the crate uses, plus the failure modes worth testing: unplug,
 * pipe stall, and a camera that stops delivering.
 *
 * POSIX only; the mock is a development aid, not something that ships.
 */

#include "toupcam.h"

#include <pthread.h>
#include <stdlib.h>
#include <string.h>
#include <time.h>

#define S_OK          0
#define S_FALSE       1
#define E_FAIL        ((HRESULT)0x80004005)
#define E_INVALIDARG  ((HRESULT)0x80070057)
#define E_GEN_FAILURE ((HRESULT)0x8007001F)

#define MOCK_WIDTH  64
#define MOCK_HEIGHT 48
#define FOURCC_RGGB (('R') | ('G' << 8) | ('G' << 16) | ('B' << 24))

struct ToupcamT {
    int open;
};

static struct {
    pthread_mutex_t lock;
    pthread_t thread;
    int streaming;
    int stop;
    int detached;
    int stalled;
    int frozen;
    int open_count;
    int fail_next_option;
    unsigned pending_frames;
    unsigned sequence;

    PTOUPCAM_EVENT_CALLBACK callback;
    void* context;

    /* Settings, remembered so the Rust side can read back what it wrote. */
    unsigned exposure_us;
    unsigned short gain;
    int options[64];
    unsigned roi[4];
    int wb[3];
} g = {
    .lock = PTHREAD_MUTEX_INITIALIZER,
    .exposure_us = 20000,
    .gain = 100,
};

static struct ToupcamT g_handle;

static const wchar_t* MODEL_NAME = L"Mock Camera";
static const ToupcamModelV2 MODEL = {
    .name = L"Mock Camera",
    .flag = TOUPCAM_FLAG_RAW8 | TOUPCAM_FLAG_RAW12 | TOUPCAM_FLAG_RAW16,
    .maxspeed = 0,
    .preview = 1,
    .still = 1,
    .maxfanspeed = 0,
    .ioctrol = 0,
    .xpixsz = 2.9f,
    .ypixsz = 2.9f,
    .res = { { MOCK_WIDTH, MOCK_HEIGHT } },
};

static void copy_wide(wchar_t* dst, const wchar_t* src, size_t max) {
    size_t i = 0;
    for (; src[i] && i + 1 < max; ++i) {
        dst[i] = src[i];
    }
    dst[i] = 0;
}

static void sleep_ms(long ms) {
    struct timespec ts = { ms / 1000, (ms % 1000) * 1000000L };
    nanosleep(&ts, NULL);
}

/* Emits events the way the SDK's own thread does. */
static void* producer(void* unused) {
    (void)unused;
    for (;;) {
        PTOUPCAM_EVENT_CALLBACK callback = NULL;
        void* context = NULL;
        unsigned event = 0;

        pthread_mutex_lock(&g.lock);
        if (g.stop) {
            pthread_mutex_unlock(&g.lock);
            return NULL;
        }
        callback = g.callback;
        context = g.context;
        if (g.detached) {
            event = TOUPCAM_EVENT_DISCONNECTED;
        } else if (g.stalled) {
            event = TOUPCAM_EVENT_NOPACKETTIMEOUT;
        } else if (!g.frozen) {
            g.pending_frames++;
            event = TOUPCAM_EVENT_IMAGE;
        }
        pthread_mutex_unlock(&g.lock);

        if (event && callback) {
            callback(event, context);
        }
        if (event == TOUPCAM_EVENT_DISCONNECTED || event == TOUPCAM_EVENT_NOPACKETTIMEOUT) {
            return NULL;
        }
        sleep_ms(10);
    }
}

const wchar_t* Toupcam_Version(void) {
    return L"mock-1.0";
}

unsigned Toupcam_EnumV2(ToupcamDeviceV2 pti[TOUPCAM_MAX]) {
    pthread_mutex_lock(&g.lock);
    int detached = g.detached;
    pthread_mutex_unlock(&g.lock);
    if (detached || !pti) {
        return 0;
    }
    memset(&pti[0], 0, sizeof(pti[0]));
    copy_wide(pti[0].displayname, L"Mock Camera", 64);
    copy_wide(pti[0].id, L"mock-0", 64);
    pti[0].model = &MODEL;
    (void)MODEL_NAME;
    return 1;
}

HToupcam Toupcam_Open(const wchar_t* id) {
    pthread_mutex_lock(&g.lock);
    if (g.detached || g_handle.open) {
        pthread_mutex_unlock(&g.lock);
        return NULL;
    }
    (void)id;
    g_handle.open = 1;
    g.open_count++;
    g.stop = 0;
    g.streaming = 0;
    g.pending_frames = 0;
    g.sequence = 0;
    g.roi[0] = 0; g.roi[1] = 0; g.roi[2] = MOCK_WIDTH; g.roi[3] = MOCK_HEIGHT;
    pthread_mutex_unlock(&g.lock);
    return &g_handle;
}

void Toupcam_Close(HToupcam h) {
    if (!h) {
        return;
    }
    Toupcam_Stop(h);
    pthread_mutex_lock(&g.lock);
    g_handle.open = 0;
    g.callback = NULL;
    g.context = NULL;
    pthread_mutex_unlock(&g.lock);
}

HRESULT Toupcam_StartPullModeWithCallback(HToupcam h, PTOUPCAM_EVENT_CALLBACK funEvent, void* ctxEvent) {
    if (!h) {
        return E_INVALIDARG;
    }
    pthread_mutex_lock(&g.lock);
    if (g.streaming) {
        pthread_mutex_unlock(&g.lock);
        return E_FAIL;
    }
    g.callback = funEvent;
    g.context = ctxEvent;
    g.stop = 0;
    g.streaming = 1;
    pthread_mutex_unlock(&g.lock);
    pthread_create(&g.thread, NULL, producer, NULL);
    return S_OK;
}

HRESULT Toupcam_Stop(HToupcam h) {
    if (!h) {
        return E_INVALIDARG;
    }
    pthread_mutex_lock(&g.lock);
    int was_streaming = g.streaming;
    g.stop = 1;
    g.streaming = 0;
    pthread_mutex_unlock(&g.lock);
    if (was_streaming) {
        /* The real SDK guarantees no callback is in flight once this returns. */
        pthread_join(g.thread, NULL);
    }
    return S_OK;
}

HRESULT Toupcam_PullImageV3(HToupcam h, void* pImageData, int bStill, int bits, int rowPitch, ToupcamFrameInfoV3* pInfo) {
    (void)bStill;
    (void)rowPitch;
    if (!h || !pImageData) {
        return E_INVALIDARG;
    }
    pthread_mutex_lock(&g.lock);
    if (g.stalled) {
        pthread_mutex_unlock(&g.lock);
        return E_GEN_FAILURE;
    }
    if (g.pending_frames == 0) {
        pthread_mutex_unlock(&g.lock);
        return S_FALSE;
    }
    g.pending_frames--;
    unsigned seq = g.sequence++;
    unsigned width = g.roi[2];
    unsigned height = g.roi[3];
    pthread_mutex_unlock(&g.lock);

    size_t samples = (size_t)width * height;
    if (bits <= 8) {
        unsigned char* out = (unsigned char*)pImageData;
        for (size_t i = 0; i < samples; ++i) {
            out[i] = (unsigned char)((i + seq) & 0xff);
        }
    } else {
        unsigned short* out = (unsigned short*)pImageData;
        for (size_t i = 0; i < samples; ++i) {
            out[i] = (unsigned short)(((i + seq) * 7) & 0xffff);
        }
    }
    if (pInfo) {
        memset(pInfo, 0, sizeof(*pInfo));
        pInfo->width = width;
        pInfo->height = height;
        pInfo->seq = seq;
        pInfo->timestamp = (unsigned long long)seq * 10000ULL;
    }
    return S_OK;
}

HRESULT Toupcam_put_Option(HToupcam h, unsigned iOption, int iValue) {
    if (!h || iOption >= 64) {
        return E_INVALIDARG;
    }
    pthread_mutex_lock(&g.lock);
    if (g.fail_next_option) {
        HRESULT hr = (HRESULT)g.fail_next_option;
        g.fail_next_option = 0;
        pthread_mutex_unlock(&g.lock);
        return hr;
    }
    g.options[iOption] = iValue;
    if (iOption == TOUPCAM_OPTION_BINNING && iValue > 0) {
        g.roi[0] = 0;
        g.roi[1] = 0;
        g.roi[2] = MOCK_WIDTH / (unsigned)iValue;
        g.roi[3] = MOCK_HEIGHT / (unsigned)iValue;
    }
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_Option(HToupcam h, unsigned iOption, int* piValue) {
    if (!h || !piValue || iOption >= 64) {
        return E_INVALIDARG;
    }
    pthread_mutex_lock(&g.lock);
    *piValue = g.options[iOption];
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_put_ExpoTime(HToupcam h, unsigned Time) {
    if (!h) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    g.exposure_us = Time;
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_ExpoTime(HToupcam h, unsigned* Time) {
    if (!h || !Time) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    *Time = g.exposure_us;
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_ExpTimeRange(HToupcam h, unsigned* nMin, unsigned* nMax, unsigned* nDef) {
    if (!h || !nMin || !nMax || !nDef) return E_INVALIDARG;
    *nMin = 32; *nMax = 60000000; *nDef = 20000;
    return S_OK;
}

HRESULT Toupcam_put_ExpoAGain(HToupcam h, unsigned short AGain) {
    if (!h) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    g.gain = AGain;
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_ExpoAGain(HToupcam h, unsigned short* AGain) {
    if (!h || !AGain) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    *AGain = g.gain;
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_ExpoAGainRange(HToupcam h, unsigned short* nMin, unsigned short* nMax, unsigned short* nDef) {
    if (!h || !nMin || !nMax || !nDef) return E_INVALIDARG;
    *nMin = 100; *nMax = 1000; *nDef = 100;
    return S_OK;
}

HRESULT Toupcam_put_Roi(HToupcam h, unsigned xOffset, unsigned yOffset, unsigned xWidth, unsigned yHeight) {
    if (!h) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    if (xWidth == 0 || yHeight == 0) {
        g.roi[0] = 0; g.roi[1] = 0; g.roi[2] = MOCK_WIDTH; g.roi[3] = MOCK_HEIGHT;
    } else if (xOffset + xWidth > MOCK_WIDTH || yOffset + yHeight > MOCK_HEIGHT) {
        pthread_mutex_unlock(&g.lock);
        return E_INVALIDARG;
    } else {
        g.roi[0] = xOffset; g.roi[1] = yOffset; g.roi[2] = xWidth; g.roi[3] = yHeight;
    }
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_Roi(HToupcam h, unsigned* pxOffset, unsigned* pyOffset, unsigned* pxWidth, unsigned* pyHeight) {
    if (!h || !pxOffset || !pyOffset || !pxWidth || !pyHeight) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    *pxOffset = g.roi[0]; *pyOffset = g.roi[1]; *pxWidth = g.roi[2]; *pyHeight = g.roi[3];
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_Size(HToupcam h, int* pWidth, int* pHeight) {
    if (!h || !pWidth || !pHeight) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    *pWidth = (int)g.roi[2];
    *pHeight = (int)g.roi[3];
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_RawFormat(HToupcam h, unsigned* nFourCC, unsigned* bitdepth) {
    if (!h || !nFourCC || !bitdepth) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    int deep = g.options[TOUPCAM_OPTION_BITDEPTH];
    pthread_mutex_unlock(&g.lock);
    *nFourCC = FOURCC_RGGB;
    *bitdepth = deep ? 16u : 8u;
    return S_OK;
}

HRESULT Toupcam_get_Temperature(HToupcam h, short* pTemperature) {
    if (!h || !pTemperature) return E_INVALIDARG;
    *pTemperature = 215; /* 21.5 C in tenths */
    return S_OK;
}

HRESULT Toupcam_get_SerialNumber(HToupcam h, char sn[32]) {
    if (!h || !sn) return E_INVALIDARG;
    strncpy(sn, "MOCK-SERIAL-0001", 32);
    sn[31] = 0;
    return S_OK;
}

HRESULT Toupcam_put_WhiteBalanceGain(HToupcam h, int aGain[3]) {
    if (!h || !aGain) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    for (int i = 0; i < 3; ++i) g.wb[i] = aGain[i];
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

HRESULT Toupcam_get_WhiteBalanceGain(HToupcam h, int aGain[3]) {
    if (!h || !aGain) return E_INVALIDARG;
    pthread_mutex_lock(&g.lock);
    for (int i = 0; i < 3; ++i) aGain[i] = g.wb[i];
    pthread_mutex_unlock(&g.lock);
    return S_OK;
}

/* --- test hooks ------------------------------------------------------ */

void Toupcam_mock_reset(void) {
    pthread_mutex_lock(&g.lock);
    g.detached = 0;
    g.stalled = 0;
    g.frozen = 0;
    g.fail_next_option = 0;
    g.pending_frames = 0;
    g.sequence = 0;
    g.exposure_us = 20000;
    g.gain = 100;
    memset(g.options, 0, sizeof(g.options));
    memset(g.wb, 0, sizeof(g.wb));
    g.roi[0] = 0; g.roi[1] = 0; g.roi[2] = MOCK_WIDTH; g.roi[3] = MOCK_HEIGHT;
    pthread_mutex_unlock(&g.lock);
}

void Toupcam_mock_unplug(void) {
    pthread_mutex_lock(&g.lock);
    g.detached = 1;
    pthread_mutex_unlock(&g.lock);
}

void Toupcam_mock_replug(void) {
    pthread_mutex_lock(&g.lock);
    g.detached = 0;
    g.stalled = 0;
    pthread_mutex_unlock(&g.lock);
}

void Toupcam_mock_stall(void) {
    pthread_mutex_lock(&g.lock);
    g.stalled = 1;
    pthread_mutex_unlock(&g.lock);
}

void Toupcam_mock_freeze(int frozen) {
    pthread_mutex_lock(&g.lock);
    g.frozen = frozen;
    pthread_mutex_unlock(&g.lock);
}

void Toupcam_mock_fail_next_option(int hresult) {
    pthread_mutex_lock(&g.lock);
    g.fail_next_option = hresult;
    pthread_mutex_unlock(&g.lock);
}

int Toupcam_mock_open_count(void) {
    pthread_mutex_lock(&g.lock);
    int count = g.open_count;
    pthread_mutex_unlock(&g.lock);
    return count;
}
