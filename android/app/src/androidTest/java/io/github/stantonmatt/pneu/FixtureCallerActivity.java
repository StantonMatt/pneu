package io.github.stantonmatt.pneu;

import android.app.Activity;
import android.content.*;
import android.net.Uri;
import android.os.Bundle;

/** Standalone test APK UID; Java avoids target APK dependencies in its separate process. */
public class FixtureCallerActivity extends Activity {
    @Override public void onCreate(Bundle state) {
        super.onCreate(state);
        if(state!=null)return; // The test caller must not replay SEND on rotation.
        String value=getIntent().getStringExtra("uri");
        Uri uri=Uri.parse(value==null?"content://io.github.stantonmatt.pneu.test.sources/image":value);
        String operation=getIntent().getStringExtra("grantOperation");
        if(operation!=null) {
            int flags=Intent.FLAG_GRANT_READ_URI_PERMISSION|Intent.FLAG_GRANT_WRITE_URI_PERMISSION;
            if("legacy".equals(operation))grantUriPermission("io.github.stantonmatt.pneu",uri,flags|Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
            else if("revoke".equals(operation))revokeUriPermission("io.github.stantonmatt.pneu",uri,flags);
            finish();return;
        }
        String action=getIntent().getStringExtra("action");
        Intent target=new Intent(action==null?Intent.ACTION_SEND:action).setType("image/png")
            .setComponent(new ComponentName("io.github.stantonmatt.pneu","io.github.stantonmatt.pneu.features.ShareActivity"))
            .putExtra(Intent.EXTRA_STREAM,uri);
        java.util.ArrayList<String> values=getIntent().getStringArrayListExtra("uris");
        if(values!=null) {
            java.util.ArrayList<Uri> uris=new java.util.ArrayList<>();for(String item:values)uris.add(Uri.parse(item));
            target.setAction(Intent.ACTION_SEND_MULTIPLE).putParcelableArrayListExtra(Intent.EXTRA_STREAM,uris);
            ClipData clip=ClipData.newRawUri("fixtures",uris.get(0));for(int index=1;index<uris.size();index++)clip.addItem(new ClipData.Item(uris.get(index)));
            target.setClipData(clip);
        }
        String text=getIntent().getStringExtra("text");
        if(text!=null){target.removeExtra(Intent.EXTRA_STREAM);target.setType("text/plain").putExtra(Intent.EXTRA_TEXT,text);}
        if(getIntent().getBooleanExtra("newTask",false))target.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK);
        if(getIntent().getBooleanExtra("grant",true))target.addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION);
        if(getIntent().getBooleanExtra("persistable",false))target.addFlags(Intent.FLAG_GRANT_PERSISTABLE_URI_PERMISSION);
        if(target.getClipData()==null && text==null)target.setClipData(ClipData.newRawUri("fixture",uri));
        try {startActivity(target);} catch(Exception ignored) {}
        if(!getIntent().getBooleanExtra("keepAlive",false))finish();
    }
}
