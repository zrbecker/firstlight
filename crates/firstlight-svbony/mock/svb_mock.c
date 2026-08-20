/*
 * A stand-in camera that implements the real SVBONY SDK header.
 *
 * Unlike a hand-written fake, this compiles against the vendor's own
 * SVBCameraSDK.h, so the declarations it satisfies are exactly the ones the
 * Rust bindings are generated from. It models the SV305C Pro closely enough
 * to be useful: the same sensor size, Bayer phase, bit depth and control
 * ranges as the real camera reports.
 *
 * It exists so the FFI layer is compiled and exercised in CI without either
 * the vendor library or hardware. It proves the plumbing, not the behaviour
 * of any real camera.
 */

#include "SVBCameraSDK.h"

#include <string.h>
#include <time.h>

#define MOCK_WIDTH 1920
#define MOCK_HEIGHT 1080
#define MOCK_MAX_BITDEPTH 12
#define MOCK_CAMERA_ID 1

static struct {
    int open;
    int video;
    int detached;
    int frozen;
    int fail_next;
    int dropped;
    unsigned sequence;
    int roi[5]; /* x, y, w, h, bin */
    SVB_IMG_TYPE image_type;
    SVB_CAMERA_MODE mode;
    long controls[32];
    SVB_BOOL autos[32];
} g;

/* Mirrors what a real SV305C Pro reports. */
static const struct {
    SVB_CONTROL_TYPE type;
    const char *name;
    long min, max, def;
    SVB_BOOL is_auto;
    SVB_BOOL writable;
} CAPS[] = {
    { SVB_EXPOSURE, "Exposure", 8, 2000000000, 30000, SVB_TRUE, SVB_TRUE },
    { SVB_GAIN, "Gain", 0, 450, 10, SVB_FALSE, SVB_TRUE },
    { SVB_WB_R, "WB_R", 0, 511, 128, SVB_TRUE, SVB_TRUE },
    { SVB_WB_G, "WB_G", 0, 511, 128, SVB_TRUE, SVB_TRUE },
    { SVB_WB_B, "WB_B", 0, 511, 128, SVB_TRUE, SVB_TRUE },
    { SVB_BLACK_LEVEL, "Offset", 0, 255, 0, SVB_FALSE, SVB_TRUE },
    { SVB_FRAME_SPEED_MODE, "Frame speed", 0, 2, 1, SVB_FALSE, SVB_TRUE },
    { SVB_GAMMA, "Gamma", 0, 1000, 100, SVB_FALSE, SVB_TRUE },
    { SVB_CURRENT_TEMPERATURE, "Current temperature", -500, 1000, 215, SVB_FALSE, SVB_FALSE },
};
#define CAPS_COUNT ((int)(sizeof(CAPS) / sizeof(CAPS[0])))

static void reset_state(void) {
    memset(&g, 0, sizeof g);
    g.roi[0] = 0;
    g.roi[1] = 0;
    g.roi[2] = MOCK_WIDTH;
    g.roi[3] = MOCK_HEIGHT;
    g.roi[4] = 1;
    g.image_type = SVB_IMG_RAW16;
    g.mode = SVB_MODE_NORMAL;
    for (int i = 0; i < CAPS_COUNT; i++) {
        g.controls[CAPS[i].type] = CAPS[i].def;
    }
}

static int initialised = 0;
static void ensure_init(void) {
    if (!initialised) {
        initialised = 1;
        reset_state();
    }
}

static int bytes_per_sample(void) {
    return (g.image_type == SVB_IMG_RAW8 || g.image_type == SVB_IMG_Y8) ? 1 : 2;
}

const char *SVBGetSDKVersion(void) { return "mock 1.13.4"; }

int SVBGetNumOfConnectedCameras(void) {
    ensure_init();
    return g.detached ? 0 : 1;
}

SVB_ERROR_CODE SVBGetCameraInfo(SVB_CAMERA_INFO *info, int index) {
    ensure_init();
    if (!info || index != 0 || g.detached) {
        return SVB_ERROR_INVALID_INDEX;
    }
    memset(info, 0, sizeof *info);
    strncpy(info->FriendlyName, "MOCK SV305C PRO", sizeof info->FriendlyName - 1);
    strncpy(info->CameraSN, "MOCK-SN-0001", sizeof info->CameraSN - 1);
    strncpy(info->PortType, "USB3.0", sizeof info->PortType - 1);
    info->DeviceID = 0;
    info->CameraID = MOCK_CAMERA_ID;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBOpenCamera(int id) {
    ensure_init();
    if (g.detached) return SVB_ERROR_CAMERA_REMOVED;
    if (id != MOCK_CAMERA_ID) return SVB_ERROR_INVALID_ID;
    if (g.open) return SVB_ERROR_INVALID_ID; /* already held */
    g.open = 1;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBCloseCamera(int id) {
    if (id != MOCK_CAMERA_ID) return SVB_ERROR_INVALID_ID;
    g.open = 0;
    g.video = 0;
    return SVB_SUCCESS;
}

static SVB_ERROR_CODE require_open(int id) {
    if (id != MOCK_CAMERA_ID) return SVB_ERROR_INVALID_ID;
    if (g.detached) return SVB_ERROR_CAMERA_REMOVED;
    if (!g.open) return SVB_ERROR_CAMERA_CLOSED;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetCameraProperty(int id, SVB_CAMERA_PROPERTY *property) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !property) return rc ? rc : SVB_ERROR_INVALID_ID;
    memset(property, 0, sizeof *property);
    property->MaxWidth = MOCK_WIDTH;
    property->MaxHeight = MOCK_HEIGHT;
    property->IsColorCam = SVB_TRUE;
    property->BayerPattern = SVB_BAYER_GR;
    property->MaxBitDepth = MOCK_MAX_BITDEPTH;
    property->IsTriggerCam = SVB_TRUE;
    property->SupportedBins[0] = 1;
    property->SupportedBins[1] = 2;
    property->SupportedVideoFormat[0] = SVB_IMG_RAW8;
    property->SupportedVideoFormat[1] = SVB_IMG_RAW16;
    property->SupportedVideoFormat[2] = SVB_IMG_END;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetCameraPropertyEx(int id, SVB_CAMERA_PROPERTY_EX *ex) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !ex) return rc ? rc : SVB_ERROR_INVALID_ID;
    memset(ex, 0, sizeof *ex);
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetSensorPixelSize(int id, float *size) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !size) return rc ? rc : SVB_ERROR_INVALID_ID;
    *size = 2.9f;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetSerialNumber(int id, SVB_SN *sn) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !sn) return rc ? rc : SVB_ERROR_INVALID_ID;
    memset(sn, 0, sizeof *sn);
    memcpy(sn->id, "MOCK-SN-0001", 12);
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetNumOfControls(int id, int *count) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !count) return rc ? rc : SVB_ERROR_INVALID_ID;
    *count = CAPS_COUNT;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetControlCaps(int id, int index, SVB_CONTROL_CAPS *caps) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !caps) return rc ? rc : SVB_ERROR_INVALID_ID;
    if (index < 0 || index >= CAPS_COUNT) return SVB_ERROR_INVALID_INDEX;
    memset(caps, 0, sizeof *caps);
    strncpy(caps->Name, CAPS[index].name, sizeof caps->Name - 1);
    strncpy(caps->Description, CAPS[index].name, sizeof caps->Description - 1);
    caps->MinValue = CAPS[index].min;
    caps->MaxValue = CAPS[index].max;
    caps->DefaultValue = CAPS[index].def;
    caps->IsAutoSupported = CAPS[index].is_auto;
    caps->IsWritable = CAPS[index].writable;
    caps->ControlType = CAPS[index].type;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetControlValue(int id, SVB_CONTROL_TYPE type, long *value, SVB_BOOL *is_auto) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (!value || (int)type < 0 || (int)type >= 32) return SVB_ERROR_INVALID_CONTROL_TYPE;
    for (int i = 0; i < CAPS_COUNT; i++) {
        if (CAPS[i].type == type) {
            *value = g.controls[type];
            if (is_auto) *is_auto = g.autos[type];
            return SVB_SUCCESS;
        }
    }
    return SVB_ERROR_INVALID_CONTROL_TYPE;
}

SVB_ERROR_CODE SVBSetControlValue(int id, SVB_CONTROL_TYPE type, long value, SVB_BOOL is_auto) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (g.fail_next) { g.fail_next = 0; return SVB_ERROR_GENERAL_ERROR; }
    for (int i = 0; i < CAPS_COUNT; i++) {
        if (CAPS[i].type == type) {
            if (!CAPS[i].writable) return SVB_ERROR_GENERAL_ERROR;
            if (value < CAPS[i].min || value > CAPS[i].max) return SVB_ERROR_GENERAL_ERROR;
            g.controls[type] = value;
            g.autos[type] = is_auto;
            return SVB_SUCCESS;
        }
    }
    return SVB_ERROR_INVALID_CONTROL_TYPE;
}

SVB_ERROR_CODE SVBSetROIFormat(int id, int x, int y, int w, int h, int bin) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (g.video) return SVB_ERROR_VIDEO_MODE_ACTIVE;
    if (bin < 1) return SVB_ERROR_INVALID_SIZE;
    if (w <= 0 || h <= 0 || w % 8 != 0 || h % 2 != 0) return SVB_ERROR_INVALID_SIZE;
    if (x + w > MOCK_WIDTH / bin || y + h > MOCK_HEIGHT / bin) return SVB_ERROR_OUTOF_BOUNDARY;
    g.roi[0] = x; g.roi[1] = y; g.roi[2] = w; g.roi[3] = h; g.roi[4] = bin;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetROIFormat(int id, int *x, int *y, int *w, int *h, int *bin) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (!x || !y || !w || !h || !bin) return SVB_ERROR_INVALID_SIZE;
    *x = g.roi[0]; *y = g.roi[1]; *w = g.roi[2]; *h = g.roi[3]; *bin = g.roi[4];
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBSetOutputImageType(int id, SVB_IMG_TYPE type) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (g.video) return SVB_ERROR_VIDEO_MODE_ACTIVE;
    if (type != SVB_IMG_RAW8 && type != SVB_IMG_RAW16) return SVB_ERROR_INVALID_IMGTYPE;
    g.image_type = type;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetOutputImageType(int id, SVB_IMG_TYPE *type) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !type) return rc ? rc : SVB_ERROR_INVALID_IMGTYPE;
    *type = g.image_type;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBSetCameraMode(int id, SVB_CAMERA_MODE mode) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    g.mode = mode;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBStartVideoCapture(int id) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    g.video = 1;
    g.sequence = 0;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBStopVideoCapture(int id) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    g.video = 0;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetVideoData(int id, unsigned char *buffer, long size, int wait_ms) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    if (!g.video) return SVB_ERROR_INVALID_SEQUENCE;
    if (!buffer) return SVB_ERROR_INVALID_SIZE;

    long needed = (long)g.roi[2] * g.roi[3] * bytes_per_sample();
    if (size < needed) return SVB_ERROR_BUFFER_TOO_SMALL;

    if (g.frozen) {
        struct timespec ts = { wait_ms / 1000, (long)(wait_ms % 1000) * 1000000L };
        nanosleep(&ts, NULL);
        return SVB_ERROR_TIMEOUT;
    }

    unsigned seq = g.sequence++;
    if (bytes_per_sample() == 1) {
        for (long i = 0; i < needed; i++) buffer[i] = (unsigned char)((i + seq) & 0xff);
    } else {
        unsigned short *out = (unsigned short *)buffer;
        for (long i = 0; i < needed / 2; i++) {
            /*
             * 12 significant bits left-aligned in the 16-bit word, which is
             * what a real SV305C Pro delivers: the low four bits are always
             * zero and the values fill the whole range.
             */
            out[i] = (unsigned short)((((i + seq) * 3) & 0x0fff) << 4);
        }
    }
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBGetDroppedFrames(int id, int *dropped) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS || !dropped) return rc ? rc : SVB_ERROR_INVALID_ID;
    *dropped = g.dropped;
    return SVB_SUCCESS;
}

SVB_ERROR_CODE SVBWhiteBalanceOnce(int id) {
    SVB_ERROR_CODE rc = require_open(id);
    if (rc != SVB_SUCCESS) return rc;
    /*
     * A real camera measures the scene and writes the result into its own
     * gains. These are the values a real SV305C Pro came back with, which
     * makes the test assert on something a camera actually produced.
     */
    g.controls[SVB_WB_R] = 213;
    g.controls[SVB_WB_G] = 128;
    g.controls[SVB_WB_B] = 240;
    return SVB_SUCCESS;
}
SVB_ERROR_CODE SVBRestoreDefaultParam(int id) { return require_open(id); }

/* --- test hooks, no counterpart in the vendor SDK -------------------- */

void SVB_mock_reset(void) { initialised = 1; reset_state(); }
void SVB_mock_unplug(void) { ensure_init(); g.detached = 1; }
void SVB_mock_replug(void) { ensure_init(); g.detached = 0; }
void SVB_mock_freeze(int frozen) { ensure_init(); g.frozen = frozen; }
void SVB_mock_fail_next_control(void) { ensure_init(); g.fail_next = 1; }
void SVB_mock_set_dropped(int dropped) { ensure_init(); g.dropped = dropped; }
