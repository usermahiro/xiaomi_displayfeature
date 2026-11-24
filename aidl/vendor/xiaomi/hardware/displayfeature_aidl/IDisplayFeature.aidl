package vendor.xiaomi.hardware.displayfeature_aidl;

import vendor.xiaomi.hardware.displayfeature_aidl.IDisplayFeatureCallback;

@VintfStability
interface IDisplayFeature {
    oneway void notifyBrightness(in int brightness);
    void registerCallback(in int displayId, in IDisplayFeatureCallback callback);
    oneway void sendMessage(in int displayId, in int mode, in String message);
    void sendPanelCommand(in String command);
    void sendPostProcCommand(in int displayId, in int command);
    void sendRefreshCommand();
    void setFeature(in int displayId, in int mode, in int value, in int cookie);
    void setFunction(in int displayId, in int mode, in int value, in int cookie);

    int getInterfaceVersion();
    String getInterfaceHash();
}
