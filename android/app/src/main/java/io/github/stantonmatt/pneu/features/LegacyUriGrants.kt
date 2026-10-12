package io.github.stantonmatt.pneu.features

import android.content.ContentResolver
import android.content.Intent

/** Shares need their temporary sender grant only during private staging.
 * Older builds persisted grants; release all of those once per process startup.
 * A stale/broken grant cannot prevent releasing the remaining entries. */
internal object LegacyUriGrants {
    fun release(resolver:ContentResolver)=PostCommit.maintain {
        resolver.persistedUriPermissions.forEach {grant->PostCommit.maintain {
            val flags=(if(grant.isReadPermission)Intent.FLAG_GRANT_READ_URI_PERMISSION else 0) or
                (if(grant.isWritePermission)Intent.FLAG_GRANT_WRITE_URI_PERMISSION else 0)
            if(flags!=0)resolver.releasePersistableUriPermission(grant.uri,flags)
        }}
    }
}
