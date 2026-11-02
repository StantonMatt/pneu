package io.github.stantonmatt.pneu.features

import android.content.Context
import io.github.stantonmatt.pneu.R
import java.text.NumberFormat

private fun Context.folderItems(n:Int)=resources.getQuantityString(R.plurals.folder_items,n,NumberFormat.getIntegerInstance().format(n))
internal fun FolderSummary.sent(context:Context,time:String,size:String)=if(unsent>0)
    context.getString(R.string.folder_sent_some,NumberFormat.getIntegerInstance().format(count),context.folderItems(count+unsent),time)
    else context.getString(R.string.folder_sent,context.folderItems(count),size,time)
internal fun FolderSkipped.notification(context:Context):String=buildList {
    val failed=unreadable+directories
    if(failed>0) {
        val shown=(names+directoryNames).take(3)
        val display=when {
            failed>shown.size->context.getString(R.string.folder_names_more,shown.joinToString(", "),NumberFormat.getIntegerInstance().format(failed-shown.size))
            shown.size>1->context.getString(R.string.folder_names_and,shown.dropLast(1).joinToString(", "),shown.last())
            else->shown.firstOrNull().orEmpty()
        }
        add(context.getString(R.string.folder_unsent,context.folderItems(failed),display))
    }
    if(links+special>0)add(if(special>0)context.getString(R.string.folder_special_skipped,NumberFormat.getIntegerInstance().format(links+special))
        else context.resources.getQuantityString(R.plurals.folder_links_skipped,links,NumberFormat.getIntegerInstance().format(links)))
}.joinToString("\n")
