// GenApi consumer acceptance for the GenTLite M1 control plane.
//
// This is the ultimate M1 check: a C++ consumer using the official GenICam
// GenApi reference implementation (VC141 MD binaries) loads gentl_gige.dll
// exactly like a third-party GenTL consumer would — GetProcAddress for every
// GenTL 1.6 export, full TL/IF/Dev open flow over GVCP against a sim_server
// on loopback — then builds a GenApi node map from the XML advertised by
// GCGetPortURL and round-trips the ExposureTime float feature through the
// producer's Port (GCReadPort/GCWritePort).
//
// Usage: test_genapi.exe <path to gentl_gige.dll>
// Exit: 0 = PASS, 1 = runtime failure (GCGetLastError text on stderr),
//       2 = usage / DLL load / missing export.
//
// Producer-side discovery of the loopback sim relies on the environment
// (set by consumer-test/build-and-run.sh):
//   GENTLITE_GVCP_PORT=<sim port>  GENTLITE_DISCOVERY_TARGETS=127.0.0.1:<sim port>

#include <windows.h>

#include <GenTL.h>
#include <GenICam.h>
#include <GenApi/NodeMapFactory.h>
#include <GenApi/PortImpl.h>

#include <cmath>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <functional>
#include <stdexcept>
#include <string>
#include <vector>

namespace {

const char* kSimDeviceId = "00:11:22:33:44:55";

// All 64 GenTL 1.6 entry points (include/GenTL.h GC_API list). Appending the
// export name to P yields the PGC* typedef from GenTL.h.
#define GENTL_API_LIST(X)                                                                           \
    X(GCGetInfo) X(GCGetLastError) X(GCInitLib) X(GCCloseLib)                                       \
    X(GCReadPort) X(GCWritePort) X(GCGetPortURL) X(GCGetPortInfo)                                   \
    X(GCRegisterEvent) X(GCUnregisterEvent)                                                         \
    X(EventGetData) X(EventGetDataInfo) X(EventGetInfo) X(EventFlush) X(EventKill)                  \
    X(TLOpen) X(TLClose) X(TLGetInfo) X(TLGetNumInterfaces) X(TLGetInterfaceID)                     \
    X(TLGetInterfaceInfo) X(TLOpenInterface) X(TLUpdateInterfaceList)                               \
    X(IFClose) X(IFGetInfo) X(IFGetNumDevices) X(IFGetDeviceID)                                     \
    X(IFUpdateDeviceList) X(IFGetDeviceInfo) X(IFOpenDevice)                                        \
    X(DevGetPort) X(DevGetNumDataStreams) X(DevGetDataStreamID) X(DevOpenDataStream)                \
    X(DevGetInfo) X(DevClose)                                                                       \
    X(DSAnnounceBuffer) X(DSAllocAndAnnounceBuffer) X(DSFlushQueue)                                 \
    X(DSStartAcquisition) X(DSStopAcquisition) X(DSGetInfo) X(DSGetBufferID)                        \
    X(DSClose) X(DSRevokeBuffer) X(DSQueueBuffer) X(DSGetBufferInfo)                                \
    X(GCGetNumPortURLs) X(GCGetPortURLInfo) X(GCReadPortStacked) X(GCWritePortStacked)              \
    X(DSGetBufferChunkData)                                                                         \
    X(IFGetParentTL) X(DevGetParentIF) X(DSGetParentDev)                                            \
    X(DSGetNumBufferParts) X(DSGetBufferPartInfo)                                                   \
    X(DSAnnounceCompositeBuffer) X(DSGetBufferInfoStacked) X(DSGetBufferPartInfoStacked)            \
    X(DSGetNumFlows) X(DSGetFlowInfo) X(DSGetNumBufferSegments) X(DSGetBufferSegmentInfo)

struct GentlApi {
#define GENTL_DECL(name) GenTL::P##name name;
    GENTL_API_LIST(GENTL_DECL)
#undef GENTL_DECL
};

GentlApi g_api;
HMODULE g_lib = nullptr;

[[noreturn]] void gc_fail(const char* what, GenTL::GC_ERROR r) {
    char text[1024] = {0};
    size_t size = sizeof(text);
    std::string detail;
    if (g_api.GCGetLastError &&
        g_api.GCGetLastError(&r, text, &size) == GenTL::GC_ERR_SUCCESS) {
        detail = text;
    }
    char msg[1400];
    std::snprintf(msg, sizeof(msg), "%s failed (GC_ERROR %d)%s%s",
                  what, static_cast<int>(r), detail.empty() ? "" : ": ", detail.c_str());
    throw std::runtime_error(msg);
}

void gc_check(GenTL::GC_ERROR r, const char* what) {
    if (r != GenTL::GC_ERR_SUCCESS) {
        gc_fail(what, r);
    }
}

// Standard GenTL string output: call with a buffer, return the first
// NUL-terminated string.
std::string gc_string(const char* what, const std::function<GenTL::GC_ERROR(char*, size_t*)>& f) {
    char buf[2048];
    size_t size = sizeof(buf);
    gc_check(f(buf, &size), what);
    return std::string(buf, strnlen(buf, size));
}

void gc_read_port(GenTL::PORT_HANDLE port, uint64_t addr, void* buf, size_t len) {
    size_t size = len;
    gc_check(g_api.GCReadPort(port, addr, buf, &size), "GCReadPort");
    if (size != len) {
        throw std::runtime_error("GCReadPort size out mismatch");
    }
}

// GenApi port adapter over the GenTL device port. Big-endian register space
// needs no swap here: the XML declares Endianess per register node.
class CGenTLPort : public GenApi::CPortImpl {
public:
    explicit CGenTLPort(GenTL::PORT_HANDLE h) : m_h(h) {}

    virtual GenApi::EAccessMode GetAccessMode() const { return GenApi::RW; }

    virtual void Read(void* pBuffer, int64_t Address, int64_t Length) {
        size_t size = static_cast<size_t>(Length);
        GenTL::GC_ERROR r = g_api.GCReadPort(m_h, static_cast<uint64_t>(Address), pBuffer, &size);
        if (r != GenTL::GC_ERR_SUCCESS) {
            gc_fail("GCReadPort", r);
        }
    }

    virtual void Write(const void* pBuffer, int64_t Address, int64_t Length) {
        size_t size = static_cast<size_t>(Length);
        GenTL::GC_ERROR r = g_api.GCWritePort(m_h, static_cast<uint64_t>(Address), pBuffer, &size);
        if (r != GenTL::GC_ERR_SUCCESS) {
            gc_fail("GCWritePort", r);
        }
    }

private:
    GenTL::PORT_HANDLE m_h;
};

// Parse `local:<file>;<address hex>;<length hex>` (GigE Vision spec
// §14.1.1; hex without prefix, case-insensitive).
void parse_local_url(const std::string& url, uint64_t* addr, size_t* len) {
    std::string lower = url;
    for (auto& c : lower) {
        c = static_cast<char>(tolower(static_cast<unsigned char>(c)));
    }
    if (lower.rfind("local:", 0) != 0) {
        throw std::runtime_error("unsupported XML URL scheme (want local:): " + url);
    }
    std::string rest = url.substr(6);
    size_t semi1 = rest.find(';');
    size_t semi2 = rest.find(';', semi1 == std::string::npos ? semi1 : semi1 + 1);
    if (semi1 == std::string::npos || semi2 == std::string::npos) {
        throw std::runtime_error("malformed local: URL: " + url);
    }
    *addr = std::stoull(rest.substr(semi1 + 1, semi2 - semi1 - 1), nullptr, 16);
    *len = std::stoull(rest.substr(semi2 + 1), nullptr, 16);
}

int run() {
    gc_check(g_api.GCInitLib(), "GCInitLib");

    GenTL::TL_HANDLE tl = nullptr;
    gc_check(g_api.TLOpen(&tl), "TLOpen");

    std::string tl_id = gc_string("TLGetInfo ID", [&](char* s, size_t* n) {
        GenTL::INFO_DATATYPE t;
        return g_api.TLGetInfo(tl, GenTL::TL_INFO_ID, &t, s, n);
    });
    std::string tl_vendor = gc_string("TLGetInfo VENDOR", [&](char* s, size_t* n) {
        GenTL::INFO_DATATYPE t;
        return g_api.TLGetInfo(tl, GenTL::TL_INFO_VENDOR, &t, s, n);
    });
    std::string tl_type = gc_string("TLGetInfo TLTYPE", [&](char* s, size_t* n) {
        GenTL::INFO_DATATYPE t;
        return g_api.TLGetInfo(tl, GenTL::TL_INFO_TLTYPE, &t, s, n);
    });
    std::printf("TL: id=%s vendor=%s tltype=%s\n", tl_id.c_str(), tl_vendor.c_str(),
                tl_type.c_str());
    if (tl_type != "GEV") {
        throw std::runtime_error("TLTYPE is not GEV");
    }

    uint32_t num_ifaces = 0;
    gc_check(g_api.TLGetNumInterfaces(tl, &num_ifaces), "TLGetNumInterfaces");
    std::printf("interfaces: %u\n", num_ifaces);

    // Find the sim camera across the interfaces (it attaches via
    // GENTLITE_DISCOVERY_TARGETS without subnet filtering, so any interface
    // may list it).
    GenTL::IF_HANDLE iface = nullptr;
    for (uint32_t i = 0; i < num_ifaces && !iface; ++i) {
        std::string iid = gc_string("TLGetInterfaceID", [&](char* s, size_t* n) {
            return g_api.TLGetInterfaceID(tl, i, s, n);
        });
        GenTL::IF_HANDLE h = nullptr;
        gc_check(g_api.TLOpenInterface(tl, iid.c_str(), &h), "TLOpenInterface");
        bool8_t changed = 0;
        gc_check(g_api.IFUpdateDeviceList(h, &changed, 200), "IFUpdateDeviceList");
        uint32_t num_devs = 0;
        gc_check(g_api.IFGetNumDevices(h, &num_devs), "IFGetNumDevices");
        for (uint32_t d = 0; d < num_devs; ++d) {
            std::string did = gc_string("IFGetDeviceID", [&](char* s, size_t* n) {
                return g_api.IFGetDeviceID(h, d, s, n);
            });
            std::printf("  iface %s device[%u] = %s\n", iid.c_str(), d, did.c_str());
            if (did == kSimDeviceId) {
                iface = h;
                break;
            }
        }
        if (!iface) {
            gc_check(g_api.IFClose(h), "IFClose");
        }
    }
    if (!iface) {
        throw std::runtime_error(
            "sim device not found; is sim_server running and "
            "GENTLITE_DISCOVERY_TARGETS=127.0.0.1:<port> set?");
    }

    GenTL::DEV_HANDLE dev = nullptr;
    gc_check(g_api.IFOpenDevice(iface, kSimDeviceId, GenTL::DEVICE_ACCESS_EXCLUSIVE, &dev),
             "IFOpenDevice");
    std::printf("device %s opened (exclusive)\n", kSimDeviceId);

    std::string dev_vendor = gc_string("DevGetInfo VENDOR", [&](char* s, size_t* n) {
        GenTL::INFO_DATATYPE t;
        return g_api.DevGetInfo(dev, GenTL::DEVICE_INFO_VENDOR, &t, s, n);
    });
    if (dev_vendor != "GenTLite") {
        throw std::runtime_error("unexpected device vendor: " + dev_vendor);
    }

    GenTL::PORT_HANDLE port = nullptr;
    gc_check(g_api.DevGetPort(dev, &port), "DevGetPort");

    // Bootstrap sanity: version register 0x00020002 (BE).
    unsigned char ver[4] = {0};
    gc_read_port(port, 0x0000, ver, sizeof(ver));
    if (!(ver[0] == 0x00 && ver[1] == 0x02 && ver[2] == 0x00 && ver[3] == 0x02)) {
        throw std::runtime_error("version register is not 0x00020002");
    }

    // Pull the device description the way every GenTL consumer must for
    // local: URLs: parse the bootstrap URL, then read the XML out of the
    // register map through the Port. (GenApi's CNodeMapFactory consumes a
    // buffer/file, not a URL.)
    std::string url = gc_string("GCGetPortURL", [&](char* s, size_t* n) {
        return g_api.GCGetPortURL(port, s, n);
    });
    std::printf("XML URL: %s\n", url.c_str());
    uint64_t xml_addr = 0;
    size_t xml_len = 0;
    parse_local_url(url, &xml_addr, &xml_len);
    std::vector<char> xml(xml_len);
    gc_read_port(port, xml_addr, xml.data(), xml.size());
    std::printf("XML: %zu bytes, header %.5s\n", xml.size(), xml.data());
    if (xml.size() < 5 || std::memcmp(xml.data(), "<?xml", 5) != 0) {
        throw std::runtime_error("XML blob does not start with <?xml");
    }

    // Build the GenApi node map. A successful CreateNodeMap is the validity
    // check of the sim's XML against the GenApi Schema 1.1 parser.
    GenApi::CNodeMapFactory factory(GenApi::ContentType_Xml, xml.data(), xml.size());
    GenApi::INodeMap* node_map = factory.CreateNodeMap("Device");
    if (!node_map) {
        throw std::runtime_error("CreateNodeMap returned NULL");
    }
    std::printf("node map created: %llu nodes\n",
                static_cast<unsigned long long>(node_map->GetNumNodes()));

    // Attach the GenTL port under the name the XML's <Port> node declares.
    CGenTLPort gen_port(port);
    if (!node_map->Connect(&gen_port)) {
        throw std::runtime_error("node map Connect(Device) failed");
    }

    GenApi::CStringPtr vendor_node(node_map->GetNode("DeviceVendorName"));
    if (!vendor_node.IsValid()) {
        throw std::runtime_error("node DeviceVendorName missing");
    }
    GenICam::gcstring vendor = vendor_node->GetValue(false, true);
    std::printf("DeviceVendorName: %s\n", vendor.c_str());
    if (vendor != "GenTLite") {
        throw std::runtime_error("unexpected DeviceVendorName");
    }

    // ExposureTime: read the default through the port, write 12345.0, then
    // read back ignoring the cache (a real GCReadPort round-trip).
    GenApi::CFloatPtr exposure(node_map->GetNode("ExposureTime"));
    if (!exposure.IsValid()) {
        throw std::runtime_error("node ExposureTime missing");
    }
    double def = exposure->GetValue(false, true);
    std::printf("ExposureTime default: %g us (min %g, max %g)\n", def, exposure->GetMin(),
                exposure->GetMax());
    if (std::fabs(def - 10000.0) > 0.5) {
        throw std::runtime_error("unexpected ExposureTime default");
    }

    exposure->SetValue(12345.0);
    double back = exposure->GetValue(false, true);
    std::printf("ExposureTime readback: %g us\n", back);
    if (std::fabs(back - 12345.0) > 0.001) {
        throw std::runtime_error("ExposureTime round-trip mismatch");
    }

    GenApi::CastToIDestroy(node_map)->Destroy();

    gc_check(g_api.DevClose(dev), "DevClose");
    gc_check(g_api.IFClose(iface), "IFClose");
    gc_check(g_api.TLClose(tl), "TLClose");
    gc_check(g_api.GCCloseLib(), "GCCloseLib");
    return 0;
}

} // namespace

int main(int argc, char** argv) {
    if (argc < 2) {
        std::fprintf(stderr, "usage: %s <path to gentl_gige.dll>\n", argv[0]);
        return 2;
    }

    g_lib = LoadLibraryA(argv[1]);
    if (!g_lib) {
        std::fprintf(stderr, "FAIL: LoadLibraryA(%s) error %lu\n", argv[1], GetLastError());
        return 2;
    }

    bool ok = true;
#define RESOLVE(name)                                                                           \
    do {                                                                                        \
        FARPROC p = GetProcAddress(g_lib, #name);                                               \
        if (!p) {                                                                               \
            std::fprintf(stderr, "FAIL: producer is missing export %s\n", #name);               \
            ok = false;                                                                         \
        } else {                                                                                \
            g_api.name = reinterpret_cast<decltype(g_api.name)>(p);                             \
        }                                                                                       \
    } while (0);
    GENTL_API_LIST(RESOLVE)
#undef RESOLVE
    if (!ok) {
        FreeLibrary(g_lib);
        return 2;
    }

    int rc = 1;
    try {
        rc = run();
    } catch (const GenICam::GenericException& e) {
        std::fprintf(stderr, "FAIL: GenApi exception: %s\n", e.what());
    } catch (const std::exception& e) {
        std::fprintf(stderr, "FAIL: %s\n", e.what());
    }

    if (rc == 0) {
        std::printf("PASS: GenApi consumer acceptance\n");
    }
    FreeLibrary(g_lib);
    return rc;
}
