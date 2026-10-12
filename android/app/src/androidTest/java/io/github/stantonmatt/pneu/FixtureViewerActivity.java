package io.github.stantonmatt.pneu;

import android.app.Activity;
import android.net.Uri;
import android.os.Bundle;
import android.widget.TextView;
import java.io.InputStream;

/** The test APK's separate UID has no READ_MEDIA_* or target APK Kotlin runtime. */
public class FixtureViewerActivity extends Activity {
    @Override public void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        String result;
        try {
            Uri uri = getIntent().getData();
            try (InputStream stream = getContentResolver().openInputStream(uri)) {
                if (stream == null || stream.read() != 137) throw new IllegalStateException("Not PNG");
            }
            result = "Original readable: " + getContentResolver().getType(uri);
        } catch (Exception error) {
            result = "Original unreadable: " + error.getClass().getSimpleName();
        }
        TextView text = new TextView(this);
        text.setText(result);
        setContentView(text);
    }
}
