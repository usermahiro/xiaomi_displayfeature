#!/system/bin/sh
MODDIR=${0%/*}

# Wait for boot completion
while [ "$(getprop sys.boot_completed)" != "1" ]; do
    sleep 2
done

# Give services time to stabilize
sleep 2

# Start service if not already running
if ! pgrep -f "vendor.xiaomi.hardware.displayfeature-service" > /dev/null; then
    if [ -x "$MODDIR/system/vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service" ]; then
        "$MODDIR/system/vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service" > /data/local/tmp/displayfeature.log 2>&1 &
    elif [ -x /vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service ]; then
        /vendor/bin/hw/vendor.xiaomi.hardware.displayfeature-service > /data/local/tmp/displayfeature.log 2>&1 &
    fi
fi