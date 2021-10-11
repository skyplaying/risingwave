package com.risingwave.planner.rules.physical.batch;

import static com.risingwave.planner.rel.logical.RisingWaveLogicalRel.LOGICAL;
import static com.risingwave.planner.rel.physical.batch.RisingWaveBatchPhyRel.BATCH_PHYSICAL;

import com.risingwave.catalog.TableCatalog;
import com.risingwave.planner.rel.logical.RwLogicalScan;
import com.risingwave.planner.rel.physical.batch.RwBatchMaterializedViewScan;
import com.risingwave.planner.rel.physical.batch.RwBatchStreamScan;
import com.risingwave.planner.rel.physical.batch.RwBatchTableScan;
import org.apache.calcite.rel.RelNode;
import org.apache.calcite.rel.convert.ConverterRule;
import org.checkerframework.checker.nullness.qual.Nullable;

/** Rule to convert Scan operator to different kinds of physical scan */
public class BatchScanConverterRule extends ConverterRule {

  public static final BatchScanConverterRule INSTANCE =
      Config.INSTANCE
          .withInTrait(LOGICAL)
          .withOutTrait(BATCH_PHYSICAL)
          .withRuleFactory(BatchScanConverterRule::new)
          .withOperandSupplier(t -> t.operand(RwLogicalScan.class).anyInputs())
          .withDescription("Converting logical filter scan to batch scan.")
          .as(Config.class)
          .toRule(BatchScanConverterRule.class);

  public BatchScanConverterRule(Config config) {
    super(config);
  }

  @Override
  public @Nullable RelNode convert(RelNode rel) {
    RwLogicalScan source = (RwLogicalScan) rel;
    TableCatalog table = source.getTable().unwrapOrThrow(TableCatalog.class);
    if (table.isStream()) {
      return RwBatchStreamScan.create(
          source.getCluster(), source.getTraitSet(), source.getTable(), source.getColumnIds());
    } else if (table.isMaterializedView()) {
      return RwBatchMaterializedViewScan.create(
          source.getCluster(), source.getTraitSet(), source.getTable(), source.getColumnIds());
    } else {
      return RwBatchTableScan.create(
          source.getCluster(), source.getTraitSet(), source.getTable(), source.getColumnIds());
    }
  }
}
