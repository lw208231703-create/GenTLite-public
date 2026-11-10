#include <pylon/PylonIncludes.h>

#include <cstdint>
#include <iostream>
#include <stdexcept>

int main() {
    Pylon::PylonInitialize();
    try {
        Pylon::CInstantCamera camera(Pylon::CTlFactory::GetInstance().CreateFirstDevice());
        std::cout << "device: " << camera.GetDeviceInfo().GetModelName() << std::endl;
        camera.MaxNumBuffer = 5;

        Pylon::CIntegerParameter exposure(camera.GetNodeMap(), "ExposureTimeAbs");
        const int64_t before = exposure.GetValue();
        exposure.SetValue(12345);
        if (exposure.GetValue() != 12345) {
            throw std::runtime_error("ExposureTimeAbs readback failed");
        }
        exposure.SetValue(before);
        std::cout << "ExposureTimeAbs: " << before << " -> 12345 -> " << before << std::endl;

        constexpr uint32_t frames = 100;
        camera.StartGrabbing(frames);
        Pylon::CGrabResultPtr result;
        uint32_t complete = 0;
        while (camera.IsGrabbing()) {
            camera.RetrieveResult(5000, result, Pylon::TimeoutHandling_ThrowException);
            if (!result->GrabSucceeded()) {
                throw std::runtime_error("frame grab failed");
            }
            Pylon::CPylonDataComponent image = result->GetFirstImageDataComponent();
            if (image.GetWidth() != 1280 || image.GetHeight() != 1024) {
                throw std::runtime_error("unexpected frame geometry");
            }
            const uint8_t* data = static_cast<const uint8_t*>(image.GetData());
            bool nonzero = false;
            for (size_t i = 0; i < image.GetDataSize(); ++i) {
                if (data[i] != 0) {
                    nonzero = true;
                    break;
                }
            }
            if (!nonzero) {
                throw std::runtime_error("zero frame");
            }
            ++complete;
        }
        if (complete != frames) {
            throw std::runtime_error("incomplete acquisition");
        }
        std::cout << "PYLON OK: " << complete << "/" << frames << " frames" << std::endl;
        Pylon::PylonTerminate();
        return 0;
    } catch (const Pylon::GenericException& e) {
        std::cerr << e.GetDescription() << std::endl;
    } catch (const std::exception& e) {
        std::cerr << e.what() << std::endl;
    }
    Pylon::PylonTerminate();
    return 1;
}
