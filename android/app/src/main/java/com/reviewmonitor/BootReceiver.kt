package com.reviewmonitor

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** 开机、应用升级后自动启动监控（魅族等系统还需要用户在「自启动管理」里允许） */
class BootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        if (intent.action == Intent.ACTION_BOOT_COMPLETED || intent.action == Intent.ACTION_MY_PACKAGE_REPLACED) {
            MonitorService.start(context)
        }
    }
}
