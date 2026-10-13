package io.github.stantonmatt.pneu;

import android.app.Activity;
import android.content.*;
import android.net.Uri;
import android.os.Bundle;

/** Standalone test APK UID; Java avoids target APK dependencies in its separate process. */
public class FixtureCallerActivity extends Activity {
    private static final int CHOOSER=1;
    private void chooserState(String state) {
        getContentResolver().call(Uri.parse("content://io.github.stantonmatt.pneu.test.sources"),"chooser-state",state,null);
    }
    private void launchFailed(String reason,boolean chooser) {
        android.util.Log.w("Pneu","share_fixture_failed: "+reason);
        if(chooser)chooserState(reason);
        finish();
    }
    @Override protected void onActivityResult(int requestCode,int resultCode,Intent data) {
        super.onActivityResult(requestCode,resultCode,data);
        if(requestCode==CHOOSER)finish();
    }
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
        if(getIntent().getBooleanExtra("clipOnly",false))target.removeExtra(Intent.EXTRA_STREAM);
        Intent launch=target;
        boolean chooser=getIntent().getBooleanExtra("chooser",false);
        if(chooser) {
            target.setComponent(null).setPackage("io.github.stantonmatt.pneu");
            // There is exactly one target. Let the real system chooser select it
            // deterministically, preserving its startActivityAsCaller attribution
            // and temporary ClipData grant without an accessibility tap race.
            launch=Intent.createChooser(target,"Share fixture")
                .putExtra(Intent.EXTRA_AUTO_LAUNCH_SINGLE_CHOICE,true);
        }
        try {
            if(chooser) {
                // Keep the sender's Activity/token alive until the chooser returns.
                startActivityForResult(launch,CHOOSER);
                chooserState("started");
                return;
            }
            startActivity(launch);
        } catch(ActivityNotFoundException ignored) {launchFailed("launch_not_found",chooser);return;}
        catch(SecurityException ignored) {launchFailed("launch_security",chooser);return;}
        catch(IllegalArgumentException ignored) {launchFailed("launch_invalid",chooser);return;}
        catch(Exception ignored) {launchFailed("launch_failed",chooser);return;}
        if(!getIntent().getBooleanExtra("keepAlive",false))finish();
    }
}
