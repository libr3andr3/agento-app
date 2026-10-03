package tech.yaya.agente

import android.os.Bundle
import androidx.appcompat.app.AppCompatActivity
import com.google.android.material.button.MaterialButton

/**
 * The neutral end of the road for a prohibited business category
 * (docs/CREDITS.md § 4): one sentence, no judgement, a way to reach
 * support, and back. Nothing was created — registration stopped before
 * the business existed.
 */
class BlockedActivity : AppCompatActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setContentView(R.layout.activity_blocked)
        findViewById<MaterialButton>(R.id.blocked_support).setOnClickListener { Support.open(this) }
        findViewById<MaterialButton>(R.id.blocked_back).setOnClickListener { finish() }
    }
}
