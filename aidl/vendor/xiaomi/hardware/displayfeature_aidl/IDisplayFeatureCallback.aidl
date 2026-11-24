package vendor.xiaomi.hardware.displayfeature_aidl;

@VintfStability
interface IDisplayFeatureCallback {
    oneway void displayfeatureInfoChanged(in int displayId, in int case_id, in float value1, in float value2, in float value3);
}
