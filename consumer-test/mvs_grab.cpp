#include <cstdio>
#include <cstring>
#include <stdexcept>
#include "MvCameraControl.h"

int main() {
    void* handle = nullptr;
    int nRet = MV_CC_Initialize();
    if (nRet != MV_OK) {
        std::fprintf(stderr, "MV_CC_Initialize: 0x%x\n", nRet);
        return 1;
    }
    try {
        MV_CC_DEVICE_INFO_LIST list;
        std::memset(&list, 0, sizeof(list));
        nRet = MV_CC_EnumDevices(
            MV_GIGE_DEVICE | MV_USB_DEVICE | MV_GENTL_GIGE_DEVICE |
                MV_GENTL_CAMERALINK_DEVICE | MV_GENTL_CXP_DEVICE |
                MV_GENTL_XOF_DEVICE | MV_GENTL_XOC_DEVICE,
            &list);
        if (nRet != MV_OK) {
            throw std::runtime_error("MV_CC_EnumDevices failed");
        }
        std::printf("MVS devices: %u\n", list.nDeviceNum);
        if (list.nDeviceNum == 0) {
            throw std::runtime_error("no MVS device");
        }
        nRet = MV_CC_CreateHandle(&handle, list.pDeviceInfo[0]);
        if (nRet != MV_OK) {
            throw std::runtime_error("MV_CC_CreateHandle failed");
        }
        nRet = MV_CC_OpenDevice(handle);
        if (nRet != MV_OK) {
            throw std::runtime_error("MV_CC_OpenDevice failed");
        }
        nRet = MV_CC_SetEnumValue(handle, "TriggerMode", 0);
        if (nRet != MV_OK) {
            throw std::runtime_error("TriggerMode failed");
        }
        nRet = MV_CC_StartGrabbing(handle);
        if (nRet != MV_OK) {
            throw std::runtime_error("MV_CC_StartGrabbing failed");
        }

        unsigned complete = 0;
        for (unsigned i = 0; i < 100; ++i) {
            MV_FRAME_OUT frame = {0};
            nRet = MV_CC_GetImageBuffer(handle, &frame, 5000);
            if (nRet != MV_OK) {
                throw std::runtime_error("MV_CC_GetImageBuffer failed");
            }
            if (frame.stFrameInfo.nExtendWidth != 1280 ||
                frame.stFrameInfo.nExtendHeight != 1024) {
                throw std::runtime_error("unexpected frame geometry");
            }
            bool nonzero = false;
            for (unsigned j = 0; j < frame.stFrameInfo.nFrameLen; ++j) {
                if (frame.pBufAddr[j] != 0) {
                    nonzero = true;
                    break;
                }
            }
            if (!nonzero) {
                throw std::runtime_error("zero frame");
            }
            nRet = MV_CC_FreeImageBuffer(handle, &frame);
            if (nRet != MV_OK) {
                throw std::runtime_error("MV_CC_FreeImageBuffer failed");
            }
            ++complete;
        }
        std::printf("MVS OK: %u/100 frames\n", complete);
        nRet = MV_CC_StopGrabbing(handle);
        if (nRet != MV_OK) {
            throw std::runtime_error("MV_CC_StopGrabbing failed");
        }
        MV_CC_CloseDevice(handle);
        MV_CC_DestroyHandle(handle);
        MV_CC_Finalize();
        return 0;
    } catch (const std::exception& e) {
        std::fprintf(stderr, "MVS error: %s\n", e.what());
    }
    if (handle) {
        MV_CC_StopGrabbing(handle);
        MV_CC_CloseDevice(handle);
        MV_CC_DestroyHandle(handle);
    }
    MV_CC_Finalize();
    return 1;
}
