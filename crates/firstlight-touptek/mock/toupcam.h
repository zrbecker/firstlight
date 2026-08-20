/*
 * A stand-in for the vendor's toupcam.h.
 *
 * It declares exactly the subset of the Touptek SDK that this crate uses,
 * with the same names, types and argument orders, so that the FFI layer can
 * be compiled and exercised by `cargo test --features mock-sdk` on a machine
 * with neither the vendor SDK nor a camera.
 *
 * IMPORTANT: this is written from the vendor's published API and is only as
 * accurate as that reading. It proves our Rust is internally consistent and
 * that the callback, pull and teardown paths work; it does NOT prove the
 * signatures match a particular SDK release. Building with `--features sdk`
 * against the real header remains the only way to check that, and testing
 * against real hardware the only way to check behaviour.
 */

#ifndef TOUPCAM_MOCK_H
#define TOUPCAM_MOCK_H

#include <stdint.h>
#include <wchar.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef int HRESULT;

#define TOUPCAM_MAX 16

/* --- events ---------------------------------------------------------- */
#define TOUPCAM_EVENT_EXPOSURE          0x0001
#define TOUPCAM_EVENT_TEMPTINT          0x0002
#define TOUPCAM_EVENT_CHROME            0x0003
#define TOUPCAM_EVENT_IMAGE             0x0004
#define TOUPCAM_EVENT_STILLIMAGE        0x0005
#define TOUPCAM_EVENT_WBGAIN            0x0006
#define TOUPCAM_EVENT_TRIGGERFAIL       0x0007
#define TOUPCAM_EVENT_BLACKBALANCE      0x0008
#define TOUPCAM_EVENT_FFC               0x0009
#define TOUPCAM_EVENT_DFC               0x000a
#define TOUPCAM_EVENT_ROI               0x000b
#define TOUPCAM_EVENT_LEVELRANGE        0x000c
#define TOUPCAM_EVENT_AUTOEXPO_CONV     0x000d
#define TOUPCAM_EVENT_AUTOEXPO_CONVFAIL 0x000e
#define TOUPCAM_EVENT_ERROR             0x0080
#define TOUPCAM_EVENT_DISCONNECTED      0x0081
#define TOUPCAM_EVENT_NOFRAMETIMEOUT    0x0082
#define TOUPCAM_EVENT_AFFEEDBACK        0x0083
#define TOUPCAM_EVENT_FOCUSPOS          0x0084
#define TOUPCAM_EVENT_NOPACKETTIMEOUT   0x0085
#define TOUPCAM_EVENT_EXPO_START        0x4000
#define TOUPCAM_EVENT_EXPO_STOP         0x4001
#define TOUPCAM_EVENT_TRIGGER_ALLOW     0x4002
#define TOUPCAM_EVENT_HEARTBEAT         0x4003
#define TOUPCAM_EVENT_FACTORY           0x8001

/* --- options --------------------------------------------------------- */
#define TOUPCAM_OPTION_RAW               0x04
#define TOUPCAM_OPTION_BITDEPTH          0x0d
#define TOUPCAM_OPTION_TRIGGER           0x10
#define TOUPCAM_OPTION_BLACKLEVEL        0x15
#define TOUPCAM_OPTION_BANDWIDTH         0x16
#define TOUPCAM_OPTION_NOFRAME_TIMEOUT   0x18
#define TOUPCAM_OPTION_BINNING           0x2b
#define TOUPCAM_OPTION_NOPACKET_TIMEOUT  0x2f

/* --- model flags ----------------------------------------------------- */
#define TOUPCAM_FLAG_RAW8       0x0000000000000004ULL
#define TOUPCAM_FLAG_RAW10      0x0000000000000008ULL
#define TOUPCAM_FLAG_RAW12      0x0000000000000010ULL
#define TOUPCAM_FLAG_RAW14      0x0000000000000020ULL
#define TOUPCAM_FLAG_RAW16      0x0000000000000040ULL
#define TOUPCAM_FLAG_MONO       0x0000000000800000ULL
#define TOUPCAM_FLAG_TEC_ONOFF  0x0000000000000400ULL

typedef struct {
    unsigned width;
    unsigned height;
} ToupcamResolution;

typedef struct {
    const wchar_t* name;
    unsigned long long flag;
    unsigned maxspeed;
    unsigned preview;
    unsigned still;
    unsigned maxfanspeed;
    unsigned ioctrol;
    float xpixsz;
    float ypixsz;
    ToupcamResolution res[16];
} ToupcamModelV2;

typedef struct {
    wchar_t displayname[64];
    wchar_t id[64];
    const ToupcamModelV2* model;
} ToupcamDeviceV2;

typedef struct {
    unsigned width;
    unsigned height;
    unsigned flag;
    unsigned seq;
    unsigned long long timestamp;
    unsigned shutterseq;
    unsigned expotime;
    unsigned short expogain;
    unsigned short blacklevel;
} ToupcamFrameInfoV3;

typedef struct ToupcamT* HToupcam;

typedef void (*PTOUPCAM_EVENT_CALLBACK)(unsigned nEvent, void* ctxEvent);

const wchar_t* Toupcam_Version(void);
unsigned Toupcam_EnumV2(ToupcamDeviceV2 pti[TOUPCAM_MAX]);
HToupcam Toupcam_Open(const wchar_t* id);
void Toupcam_Close(HToupcam h);
HRESULT Toupcam_StartPullModeWithCallback(HToupcam h, PTOUPCAM_EVENT_CALLBACK funEvent, void* ctxEvent);
HRESULT Toupcam_PullImageV3(HToupcam h, void* pImageData, int bStill, int bits, int rowPitch, ToupcamFrameInfoV3* pInfo);
HRESULT Toupcam_Stop(HToupcam h);
HRESULT Toupcam_put_Option(HToupcam h, unsigned iOption, int iValue);
HRESULT Toupcam_get_Option(HToupcam h, unsigned iOption, int* piValue);
HRESULT Toupcam_put_ExpoTime(HToupcam h, unsigned Time);
HRESULT Toupcam_get_ExpoTime(HToupcam h, unsigned* Time);
HRESULT Toupcam_get_ExpTimeRange(HToupcam h, unsigned* nMin, unsigned* nMax, unsigned* nDef);
HRESULT Toupcam_put_ExpoAGain(HToupcam h, unsigned short AGain);
HRESULT Toupcam_get_ExpoAGain(HToupcam h, unsigned short* AGain);
HRESULT Toupcam_get_ExpoAGainRange(HToupcam h, unsigned short* nMin, unsigned short* nMax, unsigned short* nDef);
HRESULT Toupcam_put_Roi(HToupcam h, unsigned xOffset, unsigned yOffset, unsigned xWidth, unsigned yHeight);
HRESULT Toupcam_get_Roi(HToupcam h, unsigned* pxOffset, unsigned* pyOffset, unsigned* pxWidth, unsigned* pyHeight);
HRESULT Toupcam_get_Size(HToupcam h, int* pWidth, int* pHeight);
HRESULT Toupcam_get_RawFormat(HToupcam h, unsigned* nFourCC, unsigned* bitdepth);
HRESULT Toupcam_get_Temperature(HToupcam h, short* pTemperature);
HRESULT Toupcam_get_SerialNumber(HToupcam h, char sn[32]);
HRESULT Toupcam_put_WhiteBalanceGain(HToupcam h, int aGain[3]);
HRESULT Toupcam_get_WhiteBalanceGain(HToupcam h, int aGain[3]);

/*
 * Test hooks. These do NOT exist in the vendor SDK; they are how the mock is
 * told to misbehave, and they are only reachable through the `mock-sdk`
 * feature.
 */
void Toupcam_mock_reset(void);
void Toupcam_mock_unplug(void);
void Toupcam_mock_replug(void);
void Toupcam_mock_stall(void);
void Toupcam_mock_freeze(int frozen);
void Toupcam_mock_fail_next_option(int hresult);
int  Toupcam_mock_open_count(void);

#ifdef __cplusplus
}
#endif

#endif /* TOUPCAM_MOCK_H */
