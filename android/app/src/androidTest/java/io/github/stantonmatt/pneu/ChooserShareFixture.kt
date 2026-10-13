package io.github.stantonmatt.pneu

import android.content.*
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Bundle
import android.os.Process
import android.os.SystemClock
import androidx.test.platform.app.InstrumentationRegistry
import org.junit.Assert.*

/** Actual system chooser and a private provider in the test APK's separate UID. */
internal object ChooserShareFixture {
    fun source(path:String)=Uri.parse("content://io.github.stantonmatt.pneu.test.sources/$path")
    fun control(context:Context,method:String):Bundle=requireNotNull(context.contentResolver.call(
        Uri.parse("content://io.github.stantonmatt.pneu.test.review"),"share-control",method,null
    ))
    fun expectedBytes(context:Context):ByteArray=requireNotNull(control(context,"image-bytes").getByteArray("bytes"))

    fun launch(context:Context,path:String) {
        val since=System.currentTimeMillis()
        try {
            val target=Intent(Intent.ACTION_SEND_MULTIPLE).setType("image/png").setPackage(context.packageName)
            assertEquals("Single-target chooser fixture must resolve only Pneu's share Activity",1,
                context.packageManager.queryIntentActivities(target,PackageManager.MATCH_DEFAULT_ONLY).size)
            val chooser=Intent.createChooser(target,"Share fixture")
            val info=context.packageManager.resolveActivity(chooser,PackageManager.MATCH_DEFAULT_ONLY)?.activityInfo
            assertNotNull("System chooser must resolve",info)
            val resolved=requireNotNull(info)
            assertNotEquals("Chooser must run outside Pneu's UID",Process.myUid(),resolved.applicationInfo.uid)
            val caller=context.packageManager.getApplicationInfo(context.packageName+".test",0)
            assertNotEquals("Chooser must run outside the source provider's UID",caller.uid,resolved.applicationInfo.uid)
            control(context,"chooser-reset")
            context.startActivity(Intent().setComponent(ComponentName(context.packageName+".test",FixtureCallerActivity::class.java.name))
                .putStringArrayListExtra("uris",arrayListOf(source(path).toString()))
                .putExtra("newTask",true).putExtra("chooser",true)
                .addFlags(Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_CLEAR_TASK))
            // Fixture failures must fail here rather than resemble a production
            // admission timeout. The operation's lifecycle is checked by callers.
            val deadline=SystemClock.elapsedRealtime()+5000
            var state=control(context,"stats").getString("chooserState").orEmpty()
            while(state.isEmpty() && SystemClock.elapsedRealtime()<deadline) {
                Thread.sleep(25);state=control(context,"stats").getString("chooserState").orEmpty()
            }
            assertEquals("Fixture must launch the real single-target chooser", "started",state)
        } finally {report("chooser-launch",since)}
    }

    /** Only fixed diagnostic codes from this attempt, never provider/content messages. */
    fun report(stage:String,since:Long) {
        val i=InstrumentationRegistry.getInstrumentation()
        val codes=try {
            i.uiAutomation.executeShellCommand("timeout 5 logcat -d -v epoch -t 200 'Pneu:W' '*:S'").use {fd->
                android.os.ParcelFileDescriptor.AutoCloseInputStream(fd).use {input->
                    val code=Regex("(?:share_rejected|share_warning|share_fixture_failed|manual_entry_rejected): [a-z_]+")
                    input.readNBytes(64*1024).toString(Charsets.UTF_8).lineSequence().mapNotNull {line->
                        val at=line.trimStart().substringBefore(' ').toDoubleOrNull() ?: return@mapNotNull null
                        if(at*1000<since)null else code.find(line)?.value
                    }.toList()
                }
            }
        }catch(_:Exception) {listOf("diagnostics_unavailable")}
        i.sendStatus(0,Bundle().apply {putString("stream","$stage Pneu reason codes: ${codes.ifEmpty {listOf("none")}.joinToString()}\n")})
        val stats=runCatching {control(i.targetContext,"stats")}.getOrNull()
        i.sendStatus(0,Bundle().apply {putString("stream",if(stats==null)"$stage provider stats unavailable\n"
            else "$stage provider opens=${stats.getInt("opens")} metadata_entered=${stats.getBoolean("entered")} chooser_state=${stats.getString("chooserState")}\n")})
    }
}
